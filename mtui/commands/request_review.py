"""The ``request_review`` command — Slack-driven review and auto-approve.

Ensures the loaded update's testreport is committed, posts its
``reports_url/<RRID>/log`` URL to a configured Slack channel asking for a
review, then (unless ``--no-watch``) blocks streaming thread replies to the
user until a 👍 reaction acks the request. On an ack, unless ``--no-approve``
is given, it drives the existing ``approve`` path — recording the best-effort
reactor as the reviewer — bound to *this* fanout template.

When the testreport already records a ``Slack Review:`` marker, the command
**resumes** that request instead of posting a duplicate: the existing thread's
replies so far are forwarded to the user and the watch continues on the same
message, so an interrupted review can be picked up later. ``--repost`` forces
a fresh post, replacing the stored marker (the old thread gets a best-effort
"superseded" reply).

The Slack message reference (channel + ts) is persisted durably in the
testreport (``set_slack_review``) so the ack survives a reload and is
re-checkable by the ``approve``/``reject`` review gate. The on-disk marker is
the source of truth: the resume decision re-reads it after the pre-commit's
``svn up`` (which may pull a colleague's newer marker), and the auto-approve
re-validates it so an ack on a superseded thread is never acted on.
"""

import subprocess
from argparse import Namespace
from contextlib import suppress
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..data_sources import SlackClient
from ..support.cancellation import current_cancel_event
from ..support.exceptions import SlackError
from ..support.misc import requires_update
from ..support.spinner import spinner
from ..test_reports.svn_io import (
    TemplateFormatError,
    svn_commit_testreport,
    svn_update_testreport,
)
from . import Command
from .approve import Approve

logger = getLogger("mtui.command.request_review")


class RequestReview(Command):
    """Requests a Slack review of the loaded update and auto-approves on ack.

    Commits the testreport (auto-committing first if needed), posts its log
    URL to the configured Slack channel — or, when a ``Slack Review:`` marker
    is already recorded, **resumes watching that existing request** instead of
    posting a duplicate — then watches the thread: replies are streamed as
    they arrive (on a resume, the thread's history so far is forwarded first)
    and a 👍 reaction is treated as review approval. On ack the update is
    approved with the reactor recorded as reviewer.

    Use ``--no-watch`` to post and return without watching, ``--no-approve``
    to watch and report the ack without approving, or ``--repost`` to post a
    fresh request even when one is already recorded.
    """

    command = "request_review"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "--no-watch",
            dest="no_watch",
            action="store_true",
            help="post the review request and return without watching the thread",
        )
        parser.add_argument(
            "--no-approve",
            dest="no_approve",
            action="store_true",
            help="watch and report the ack, but do not approve the update",
        )
        parser.add_argument(
            "--repost",
            dest="repost",
            action="store_true",
            help="post a fresh review request even when one is already "
            "recorded, replacing the stored marker (the old thread gets a "
            "'superseded' reply)",
        )
        parser.add_argument(
            "-g",
            "--group",
            nargs="?",
            action="append",
            help="Group wanted to approve\n Not valid for Gitea Workflow",
        )
        parser.add_argument(
            "-u",
            "--user",
            action="store",
            default="",
            help="User override for gitea workflow (Gitea only)",
        )
        cls._add_template_arg(parser)

    def run(self) -> None:
        """Refuse an unscoped multi-template ``--repost``, then fan out.

        ``--repost`` replaces each template's recorded review; blindly doing
        that for every loaded template orphans live threads wholesale, so it
        must be scoped to one template explicitly. Checked here — once, before
        the fan-out — rather than per template in :meth:`__call__`, so the
        refusal prints a single line and the fan-out never logs a misleading
        "succeeded on" for templates nothing was done to.
        """
        if self.args.repost and len(self._resolve_templates()) > 1:
            self.println(
                "--repost acts on every fanned-out template and would replace "
                "each one's recorded review request; scope it with -T RRID."
            )
            return
        super().run()

    @requires_update
    def __call__(self) -> None:
        """Commit, post to (or resume on) Slack, watch, and approve on 👍."""
        # 1) Auto-commit so the qam.suse.de /log mirror reflects the report we
        #    are asking people to review; abort before posting if it fails.
        try:
            svn_commit_testreport(
                self.metadata.report_wd(),
                self.config.install_logs,
                ["-m", "commit before review request"],
            )
        except subprocess.CalledProcessError as e:
            self.println(f"Failed to commit testreport, not requesting review: {e}")
            return

        # 1b) The commit above ran ``svn up``, which may have pulled in a
        #     colleague's newer / reposted / removed marker — the file, not the
        #     load-time snapshot, decides whether to resume, and the in-memory
        #     attribute is synced so the approve/reject gate sees the same ts.
        existing = self.metadata.get_slack_review()
        self.metadata.slack_review = existing

        resumed = existing is not None and not self.args.repost
        if resumed:
            # 2a) Resume the recorded request: no new post, no marker rewrite.
            #     The watch below starts at the top of the thread, so replies
            #     that arrived while nobody was watching are forwarded now.
            channel, ts = existing
            if self.args.no_watch:
                self.println(
                    f"Review request for {self.metadata.rrid} already posted "
                    f"({channel}/{ts}); not watching. Pass --repost to post "
                    "a fresh one."
                )
                return
            try:
                client = SlackClient(self.config)
            except SlackError as e:
                self.println(f"Slack client unavailable: {e}")
                return
            self.println(
                f"Resuming existing review request for {self.metadata.rrid} "
                f"at {channel}/{ts} (pass --repost for a fresh one)."
            )
        else:
            # 2b) Post a fresh request. A template that cannot record the
            #     review marker would leave a dangling Slack message whose ack
            #     the approve gate can never see; refuse before posting.
            if not self.metadata.has_slack_review_anchor():
                self.println(
                    f"Testreport for {self.metadata.rrid} has no "
                    "'Test Plan Reviewer:' line to anchor the Slack review "
                    "marker; fix the template before requesting review."
                )
                return

            url = self.metadata._testreport_url()  # noqa: SLF001 -- shared report URL helper
            channel = self.config.slack_channel

            try:
                client = SlackClient(self.config)
                ts = client.chat_postMessage(
                    channel,
                    f"Please review {self.metadata.rrid}: {url}",
                )
            except SlackError as e:
                self.println(f"Failed to post Slack review request: {e}")
                return

            # 3) Persist the marker so the ack survives a reload, then commit
            #    it. The anchor was checked before posting, but the file can
            #    still turn on us (concurrent edit, read-only checkout) —
            #    report it instead of dying with the request already live.
            try:
                self.metadata.set_slack_review(channel, ts)
            except (TemplateFormatError, OSError) as e:
                logger.error(
                    "Unrecorded Slack review request %s/%s: %s", channel, ts, e
                )
                # The just-posted message is recorded nowhere: a later re-run
                # cannot breadcrumb it, so void it now while we still hold the
                # ts — otherwise a reviewer acks a message nobody watches.
                with suppress(SlackError):
                    client.chat_postMessage(
                        channel,
                        f"This review request for {self.metadata.rrid} could "
                        "not be recorded and is void; please ack the "
                        "replacement request instead.",
                        thread_ts=ts,
                    )
                self.println(
                    f"Posted review request {channel}/{ts} but could not "
                    f"record the marker: {e}. The approve gate will not see "
                    "this review; fix the template and re-run request_review "
                    f"with --repost (scoped with -T {self.metadata.rrid} when "
                    "several templates are loaded)."
                )
                return
            try:
                svn_commit_testreport(
                    self.metadata.report_wd(),
                    self.config.install_logs,
                    ["-m", f"Add Slack Review: {channel}/{ts}"],
                )
            except subprocess.CalledProcessError as e:
                logger.error("Failed to commit Slack review marker: %s", e)

            # A reposted request supersedes the old thread; leave a breadcrumb
            # there so reviewers do not ack a message nobody watches. Best
            # effort only — the old message may have been deleted.
            if existing is not None:
                old_channel, old_ts = existing
                with suppress(SlackError):
                    client.chat_postMessage(
                        old_channel,
                        f"Superseded by a fresh review request for "
                        f"{self.metadata.rrid}: {channel}/{ts}",
                        thread_ts=old_ts,
                    )
                self.println(
                    f"Superseded previous review request {old_channel}/{old_ts}."
                )

            if self.args.no_watch:
                self.println(
                    f"Posted review request for {self.metadata.rrid}; not watching."
                )
                return

        # 4) Block on the thread until acked, timed out, or interrupted. A review
        # can take hours; the wait runs to slack_watch_timeout (a full working day
        # by default). In the REPL press Ctrl-C to stop watching; over MCP call
        # this with background=true and poll the job instead of blocking.
        self.println(
            f"Watching Slack for a 👍 on {self.metadata.rrid} "
            "(this can take a while; Ctrl-C to stop)."
        )
        with spinner(f"Waiting for review of {self.metadata.rrid}") as is_stopped:
            outcome = client.wait_for_ack(
                channel,
                ts,
                on_reply=self.println,
                should_stop=is_stopped,
                interval=self.config.slack_poll_interval,
                timeout=self.config.slack_watch_timeout,
                # Set by the MCP session when the job is cancelled or the
                # client disconnects, so the watch (and its worker thread)
                # exits promptly instead of polling on unobserved — and
                # possibly auto-approving — for hours. None in the REPL.
                cancel_event=current_cancel_event.get(),
            )

        if not outcome.acked:
            if outcome.unreachable and resumed:
                # On a resume this is the guaranteed outcome of a deleted
                # message, and every plain re-run resumes the same dead ts —
                # the remedy must be in the failure line itself.
                self.println(
                    f"Review watch for {self.metadata.rrid} failed (Slack "
                    f"unreachable, or the review message {channel}/{ts} was "
                    "deleted); re-run with --repost to post a fresh request."
                )
            elif outcome.unreachable:
                self.println(
                    f"Review watch for {self.metadata.rrid} failed (Slack unreachable)"
                )
            else:
                self.println(
                    f"No review ack for {self.metadata.rrid} (timed out or stopped)"
                )
            return

        self.println(f"Review acked by {outcome.reviewer} (best-effort reactor)")
        if self.args.no_approve:
            return

        # 4b) Approve guards. A cancelled job's watch may observe an ack in
        #     its final in-flight poll after job_cancel already returned — a
        #     cancelled watch must never mutate state. And the ack must still
        #     belong to the marker the report records: a --repost or a
        #     colleague's newer request supersedes this watch. The colleague's
        #     repost lives in the repository, not this checkout — the watch
        #     ran for hours since the last ``svn up`` — so refresh first
        #     (best-effort: an offline refresh falls back to the local state).
        cancel = current_cancel_event.get()
        if cancel is not None and cancel.is_set():
            self.println(
                f"Watch for {self.metadata.rrid} was cancelled; the observed "
                "ack was not acted on."
            )
            return
        try:
            svn_update_testreport(self.metadata.report_wd())
        except (subprocess.CalledProcessError, OSError) as e:
            logger.warning("Could not refresh the testreport before approving: %s", e)
        if self.metadata.get_slack_review() != (channel, ts):
            self.println(
                f"Review request {channel}/{ts} for {self.metadata.rrid} was "
                "superseded; not approving from this watch."
            )
            return

        # 5) Approve THIS fanout template (not prompt's active one) by binding
        #    the already-resolved metadata/targets onto a fresh Approve. Its
        #    __call__ runs the Slack review gate, which passes because the ack
        #    was just persisted and is still live.
        appr = Approve(
            Namespace(
                reviewer=outcome.reviewer,
                group=self.args.group,
                user=self.args.user,
                template=None,
                all_templates=False,
                force=False,
            ),
            self.config,
            self.sys,
            self.prompt,
        )
        appr.metadata = self.metadata
        appr.targets = self.targets
        appr.__call__()

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("--no-watch",),
                ("--no-approve",),
                ("--repost",),
                ("-g", "--group"),
                ("-u", "--user"),
                *template_completion(state),
            ],
            line,
            text,
        )
