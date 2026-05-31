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

from ..argparse import ArgumentParser
from ..commands import Command
from ..completion import complete_choices
from ..connector import OSC, Gitea
from ..exceptions import GiteaError
from ..misc import requires_update
from ..types import RequestKind

logger = getLogger("mtui.command.apicalls")


class BaseApiCall(Command, ABC):
    """An abstract base class for commands that interact with backend APIs."""

    # For a Product Increment, ``assign`` locks all reference hosts and the
    # end-of-testing operations unlock them. Subclasses set this to "lock"
    # or "unlock"; ``None`` (the default, e.g. ``comment``) does neither.
    _pi_action: ClassVar[str | None] = None

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds common arguments to the command's argument parser."""
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
        return complete_choices([("-g", "--group"), ("-u", "--user")], line, text)


@final
class Assign(BaseApiCall):
    """A command to assign a review request to a user or group."""

    command = "assign"
    _pi_action = "lock"

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
            [("-g", "--group"), ("-u", "--user"), ("-f", "--force")], line, text
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

    def osc(self) -> None:
        """Rejects the request in OSC."""
        logger.info("Reject request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.reject(self.args.group, self.args.reason, self.args.message)

    def gitea(self) -> None:
        """Rejects the pull request in Gitea."""
        logger.info("Reject PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.reject(self.args.reason, self.args.user, self.args.message)
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

    def osc(self) -> None:
        """Adds a comment to the request in OSC."""
        comment = input("Comment: ")
        osc = OSC(self.config, self.metadata.rrid)
        osc.comment(comment)

    def gitea(self) -> None:
        """Adds a comment to the pull request in Gitea."""
        comment = input("Comment: ")
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.comment(comment)
        except GiteaError as e:
            logger.error(e)
