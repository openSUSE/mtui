"""Commands for interacting with backend APIs (OSC and Gitea).

This module defines the :class:`BaseApiCall` base class, which handles the
dispatch logic between the OSC and Gitea backends, along with the concrete
`assign`, `unassign`, `reject`, and `comment` commands. The `approve`
command lives in :mod:`mtui.commands.approve`.
"""

from abc import ABC, abstractmethod
from argparse import REMAINDER
from logging import getLogger
from typing import ClassVar, final

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..cli.term import ask_user
from ..data_sources import OSC, Gitea, TeReGen
from ..support.exceptions import FailedSlackCallError, GiteaError
from ..support.misc import requires_update
from ..types import RequestKind
from . import Command

logger = getLogger("mtui.command.apicalls")


def require_slack_review(command: "BaseApiCall") -> None:
    """Refuse an approve/reject unless the report carries a live Slack ack.

    ``request_review`` persists the Slack message reference (channel + ts) in
    the testreport via :meth:`TestReport.set_slack_review`. This gate reads
    the marker back **from the template file** (not the load-time snapshot —
    any ``svn up`` may have pulled in a colleague's newer or reposted marker)
    and re-queries Slack live so the 👍 ack is confirmed to still be present
    at approve/reject time (durable and re-checkable across sessions).

    The marker is plain text in the testreport, so anything that can edit the
    template (``edit``, the MCP testreport write tools) can point it at an
    arbitrary Slack message. Before trusting any reaction the gate therefore
    BINDS the marker to this update:

    1. the referenced message must exist and be readable in the marker's
       channel (fetched via ``conversations.replies``; the parent/request
       message is the first element),
    2. the parent message text must name this update's RRID verbatim —
       ``request_review`` always includes the RRID in the request message it
       posts, so a message that does not name this RRID is not a review
       request for this update,
    3. only then are the parent's reactions checked for a 👍 ack (reaction
       names are normalized so skin-toned variants like ``+1::skin-tone-3``
       still count).

    Channel binding: ``config.slack_channel`` may hold a channel *name* while
    the marker records the canonical channel *id* Slack returned when the
    request was posted, so a hard equality check against the config would
    falsely refuse legitimate reviews. The fetch + RRID text check above
    establishes the binding instead; a config mismatch is only surfaced as a
    warning for the operator.

    Raises:
        FailedSlackCallError: if no Slack review is recorded (the user is
            pointed at ``request_review``), the referenced message cannot be
            fetched, the message is not this update's review request, or the
            live reactions no longer carry a 👍.

    """
    review = command.metadata.get_slack_review()
    if review is None:
        raise FailedSlackCallError(
            "No Slack review recorded for this update; run 'request_review' first."
        )

    # Imported lazily to avoid an import cycle at module load time.
    from ..data_sources import SlackClient
    from ..data_sources.slack import is_ack_reaction

    channel, ts = review
    rrid = str(command.metadata.rrid)

    if channel != command.config.slack_channel:
        # Not a refusal: the marker usually stores the resolved channel id
        # while the config may hold the channel name (see the docstring). The
        # binding is verified via the message fetch below.
        logger.warning(
            "Slack review marker channel %s differs from configured "
            "slack_channel %s; verifying the marker via the message itself.",
            channel,
            command.config.slack_channel,
        )

    try:
        messages = SlackClient(command.config).conversations_replies(channel, ts)
    except FailedSlackCallError as e:
        raise FailedSlackCallError(
            f"Slack review message {channel}/{ts} could not be fetched: {e}. "
            "The recorded marker may be stale; re-run 'request_review'."
        ) from e
    if not messages:
        raise FailedSlackCallError(
            f"Slack review message {channel}/{ts} does not exist; "
            "the recorded marker is stale. Re-run 'request_review'."
        )

    parent = messages[0]
    if rrid not in parent.get("text", ""):
        raise FailedSlackCallError(
            f"Slack review message {channel}/{ts} does not mention {rrid}; "
            "the recorded marker does not match this update (forged or "
            "stale). Re-run 'request_review' to post a genuine request."
        )

    # is_ack_reaction normalizes skin-toned variants ("+1::skin-tone-3").
    reactions = parent.get("reactions", [])
    if not any(is_ack_reaction(r.get("name") or "") for r in reactions):
        raise FailedSlackCallError(
            f"Slack review {channel}/{ts} has no 👍 ack; refusing. "
            "Have a reviewer react with 👍 first."
        )


class BaseApiCall(Command, ABC):
    """An abstract base class for commands that interact with backend APIs."""

    # API calls act on a single template's RRID; with several templates loaded
    # they fan out (one backend call per template) and honour ``-T/--template``.
    scope = "fanout"

    # For a Product Increment, ``assign`` locks all reference hosts and the
    # end-of-testing operations unlock them. Subclasses set this to "lock"
    # or "unlock"; ``None`` (the default, e.g. ``comment``) does neither.
    _pi_action: ClassVar[str | None] = None

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds common arguments to the command's argument parser."""
        cls._add_template_arg(parser)
        parser.add_argument(
            "-g",
            "--group",
            nargs="?",
            action="append",
            help=f"Group wanted to {cls.command}\n Not valid for Gitea Workflow",
        )
        parser.add_argument(
            "-u",
            "--user",
            action="store",
            default="",
            help="User override for gitea workflow (Gitea only)",
        )

    @property
    def _is_gitea_workflow(self) -> bool:
        """Determines if the request should be handled by the Gitea API."""
        rrid = self.metadata.rrid
        return rrid.kind is RequestKind.SLFO and rrid.maintenance_id != "1.1"

    @requires_update
    def __call__(self) -> None:
        """The main entry point for the command."""
        if self._is_gitea_workflow:
            self.gitea()
        else:
            self.osc()
        self._pi_autolock()
        self._after()

    def _after(self) -> None:
        """Hook run after the API action; overridden by subclasses (no-op here)."""

    def _show_priority_deadline(self) -> None:
        """Print the loaded update's priority and deadline, if available.

        Sourced from the TeReGen report API. Best-effort context for the tester
        picking up an update: silent when TeReGen has nothing for this request.
        """
        priority, deadline = TeReGen(self.config).priority_deadline(self.metadata.rrid)
        if priority is None and deadline is None:
            return
        self.println(
            f"TeReGen: priority {priority if priority is not None else '?'}, "
            f"deadline {deadline or '?'}"
        )

    def _pi_autolock(self) -> None:
        """Locks/unlocks reference hosts around PI testing.

        On ``assign`` of a Product Increment, lock every connected
        reference host with a comment naming the request, and remember the
        comment so hosts added later (via ``add_host``) are locked too. On
        ``unassign`` / ``approve`` / ``reject``, unlock this session's
        locks. No-op unless the request is a PI and ``lock_pi_autolock`` is
        enabled.
        """
        if self._pi_action is None or not self.config.lock_pi_autolock:
            return
        if self.metadata.rrid.kind is not RequestKind.PI:
            return

        if self._pi_action == "lock":
            comment = f"testing of {self.metadata.rrid}"
            self.metadata.lock_comment = comment
            logger.info("Locking reference hosts for %s", self.metadata.rrid)
            self.targets.lock(comment)
        else:  # "unlock"
            logger.info(
                "Unlocking reference hosts after %s of %s",
                self.command,
                self.metadata.rrid,
            )
            self.targets.unlock()
            self.metadata.lock_comment = ""

    @abstractmethod
    def osc(self) -> None:
        """Provides OSC-specific logic."""
        raise NotImplementedError

    @abstractmethod
    def gitea(self) -> None:
        """Provides Gitea-specific logic."""
        raise NotImplementedError

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-g", "--group"), ("-u", "--user"), *template_completion(state)],
            line,
            text,
        )


@final
class Assign(BaseApiCall):
    """A command to assign a review request to a user or group."""

    command = "assign"
    _pi_action = "lock"

    def _after(self) -> None:
        """Surface the update's priority + deadline when picking it up."""
        self._show_priority_deadline()

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        super()._add_arguments(parser)
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="Force assign review to user in Gitea PR, even there isn't open group",
        )

    def osc(self) -> None:
        """Assigns the request in OSC."""
        logger.info("Assign request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.assign(self.args.group)

    def gitea(self) -> None:
        """Assigns the pull request in Gitea."""
        logger.info("Assign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.assign(self.args.user, self.args.force)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-g", "--group"),
                ("-u", "--user"),
                ("-f", "--force"),
                *template_completion(state),
            ],
            line,
            text,
        )


@final
class Unassign(BaseApiCall):
    """A command to unassign a review request."""

    command = "unassign"
    _pi_action = "unlock"

    def osc(self) -> None:
        """Unassigns the request in OSC."""
        logger.info("Unassign request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.unassign(self.args.group)

    def gitea(self) -> None:
        """Unassigns the pull request in Gitea."""
        logger.info("Unassign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.unassign(self.args.user)
        except GiteaError as e:
            logger.error(e)


@final
class Reject(BaseApiCall):
    """A command to reject a review request."""

    command = "reject"
    _pi_action = "unlock"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        super()._add_arguments(parser)
        parser.add_argument(
            "-r",
            "--reason",
            required=True,
            choices=[
                "admin",
                "retracted",
                "build_problem",
                "not_fixed",
                "regression",
                "false_reject",
                "tracking_issue",
            ],
            help="Reason to reject update, required",
        )
        parser.add_argument(
            "-m",
            "--message",
            nargs=REMAINDER,
            help="Message to use for rejection-comment."
            + "Always as last of command, it takes remainder of command",
        )

    @requires_update
    def __call__(self) -> None:
        """Reject the request after the Slack review gate.

        Mirrors :class:`~mtui.commands.approve.Approve`: require a live Slack
        👍 ack before delegating to the shared backend-dispatch path.
        """
        require_slack_review(self)
        super().__call__()

    @property
    def _message(self) -> str:
        """The rejection message as a single string.

        ``--message`` uses ``nargs=REMAINDER``, so ``self.args.message`` is a
        list of words (or ``None`` when omitted). It must be joined before it
        reaches ``osc.reject``/``gitea.reject``, which pass it to
        ``shlex.quote`` — handing those a list raises ``TypeError`` and aborts
        the reject before it is sent.
        """
        return " ".join(self.args.message) if self.args.message else ""

    def osc(self) -> None:
        """Rejects the request in OSC."""
        logger.info("Reject request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.reject(self.args.group, self.args.reason, self._message)

    def gitea(self) -> None:
        """Rejects the pull request in Gitea."""
        logger.info("Reject PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.reject(self.args.reason, self.args.user, self._message)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-g", "--group"),
                ("-r", "--reason"),
                ("-m", "--message"),
                ("-u", "--user"),
                (
                    "admin",
                    "retracted",
                    "build_problem",
                    "not_fixed",
                    "regression",
                    "false_reject",
                    "tracking_issue",
                ),
                *template_completion(state),
            ],
            line,
            text,
        )


@final
class Comment(BaseApiCall):
    """A command to add a comment to a review request."""

    command = "comment"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)

    def osc(self) -> None:
        """Adds a comment to the request in OSC."""
        comment = ask_user("Comment: ")
        osc = OSC(self.config, self.metadata.rrid)
        osc.comment(comment)

    def gitea(self) -> None:
        """Adds a comment to the pull request in Gitea."""
        comment = ask_user("Comment: ")
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.comment(comment)
        except GiteaError as e:
            logger.error(e)
