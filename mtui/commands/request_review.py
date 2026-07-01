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

An unscoped multi-template invocation runs in **two phases**: every template's
request is posted (or resumed) first, then ONE combined watch polls all of
them round-robin, approving each as its ack arrives, until every request is
resolved or the shared ``slack_watch_timeout`` deadline expires. A watch can
therefore never delay another template's post, and Ctrl-C mid-watch reports a
per-template status summary instead of silently dropping the other templates
(their posts and markers survive; a re-run resumes them). A ``-T``-scoped
invocation — including each per-template background job the MCP session mints
for a fanned-out command — keeps the classic single-template blocking flow.
"""

import subprocess
import threading
import time
from argparse import Namespace
from contextlib import suppress
from dataclasses import dataclass
from logging import getLogger
from typing import Any

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..data_sources import ReviewOutcome, SlackClient
from ..support.cancellation import current_cancel_event
from ..support.exceptions import FailedSlackCallError, SlackError
from ..support.messages import FanOutError
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

#: Consecutive failed poll cycles after which one pending request in the
#: combined fan-out watch is declared unreachable. Mirrors
#: ``mtui.data_sources.slack._MAX_POLL_FAILURES`` so the round-robin watch
#: degrades exactly like the single-template ``wait_for_ack``.
_MAX_POLL_FAILURES = 3


@dataclass
class _PendingWatch:
    """One template's live review request, awaiting its 👍.

    Carries the template's own ``metadata``/``targets`` binding so the ack
    handling (supersede guard, auto-approve) acts on *this* template even when
    the command object has since been rebound to another fan-out template.
    ``status`` is a human-readable progress note surfaced by the Ctrl-C
    per-template summary; ``last_seen``/``failures`` are the round-robin poll
    cursor and consecutive-failure counter (see ``_poll_watch``).
    """

    metadata: Any
    targets: Any
    client: SlackClient
    channel: str
    ts: str
    resumed: bool
    status: str
    last_seen: int = 1
    failures: int = 0


class RequestReview(Command):
    """Requests a Slack review of the loaded update and auto-approves on ack.

    Commits the testreport (auto-committing first if needed), posts its log
    URL to the configured Slack channel — or, when a ``Slack Review:`` marker
    is already recorded, **resumes watching that existing request** instead of
    posting a duplicate — then watches the thread: replies are streamed as
    they arrive (on a resume, the thread's history so far is forwarded first)
    and a 👍 reaction is treated as review approval. On ack the update is
    approved with the reactor recorded as reviewer.

    With several templates loaded, every template's request is posted first
    and a single combined watch then polls all of them, approving each as its
    ack arrives.

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

    @staticmethod
    def _cancel_requested() -> bool:
        """Whether the job's cooperative cancel event is set (``False`` in the REPL).

        Polled before every externally visible side effect so a cancelled MCP
        job stops promptly and truthfully instead of committing, posting, or
        approving after ``job_cancel`` already reported "cancelled".
        """
        cancel = current_cancel_event.get()
        return cancel is not None and cancel.is_set()

    def run(self) -> None:
        """Two-phase fan-out: post every template's request, then watch them all.

        ``--repost`` replaces each template's recorded review; blindly doing
        that for every loaded template orphans live threads wholesale, so it
        must be scoped to one template explicitly. Checked here — once, before
        the fan-out — so the refusal prints a single line.

        A single resolved template (including every ``-T``-scoped per-template
        background job the MCP session mints) runs the classic blocking flow
        via :meth:`__call__`, byte-for-byte like ``Command.run``. With more
        than one template, PHASE 1 posts (or resumes) each template's request
        — collecting per-template failures exactly like the base fan-out —
        and PHASE 2 runs ONE combined watch over every pending request, so no
        template's post waits hours behind another template's watch. Ctrl-C
        stops the watch and prints a per-template status summary; the posted
        requests and their markers survive and a re-run resumes them.
        """
        resolved = self._resolve_templates()
        if self.args.repost and len(resolved) > 1:
            self.println(
                "--repost acts on every fanned-out template and would replace "
                "each one's recorded review request; scope it with -T RRID."
            )
            return

        if len(resolved) <= 1:
            # Single template: keep the historical single-call dispatch (and
            # its error contract) exactly as ``Command.run`` would.
            report = resolved[0]
            self.metadata = report
            self.targets = report.targets
            self.__call__()
            return

        failures: list[tuple[str, BaseException]] = []
        watches: list[_PendingWatch] = []
        unprocessed = [str(report.id) for report in resolved]
        try:
            # PHASE 1 — post (or resume) every template's request BEFORE any
            # watch, mirroring the base fan-out's banner + collect-and-continue
            # error reporting, so template N+1's Slack request is live within
            # seconds instead of hours behind template N's watch.
            for report in resolved:
                rrid = str(report.id)
                self.metadata = report
                self.targets = report.targets
                self.display.template_banner(rrid)
                try:
                    item = self._post_or_resume()
                except Exception as exc:  # noqa: BLE001 - collect & continue
                    logger.error("%s failed on %s: %s", self.command, rrid, exc)
                    failures.append((rrid, exc))
                else:
                    if item is not None:
                        watches.append(item)
                unprocessed.remove(rrid)

            # PHASE 2 — one combined watch over every still-pending request
            # (empty under --no-watch or when every template resolved early).
            if watches:
                self._watch_all(watches)
        except KeyboardInterrupt:
            # A BaseException would bypass the per-template ``except`` above
            # and silently drop the other templates. The posts and markers
            # already made are durable — report where each template stands
            # instead of losing them.
            self._report_interrupted(watches, unprocessed)
            return

        done = {rrid for rrid, _ in failures}
        ok = [str(report.id) for report in resolved if str(report.id) not in done]
        if ok:
            logger.info("%s succeeded on: %s", self.command, ", ".join(ok))
        if failures:
            raise FanOutError(failures)

    @requires_update
    def __call__(self) -> None:
        """Commit, post to (or resume on) Slack, watch, and approve on 👍."""
        item = self._post_or_resume()
        if item is None:
            return
        self._watch_one(item)

    def _post_or_resume(self) -> _PendingWatch | None:
        """Commit the report and post (or resume) its Slack review request.

        Runs the per-template side-effect phase for ``self.metadata``: the
        pre-post ``svn`` commit, the resume-or-post decision, the durable
        marker write and its commit, and the ``--repost`` supersede
        breadcrumb. The job's cancel event is polled before every externally
        visible side effect so a cancelled job stops cleanly mid-sequence.

        Returns:
            A :class:`_PendingWatch` for the live request when a watch should
            follow, or ``None`` when the flow ended here (``--no-watch``, a
            refusal, a failure — each already reported to the user).

        """
        # 0) Fail fast on missing Slack configuration: an empty channel would
        #    otherwise reach the API and die with a misleading
        #    ``channel_not_found``; an empty token with a bare client error.
        if not self.config.slack_token:
            self.println(
                "Not requesting review: [slack] token is not configured "
                "(set it, or the SLACK_TOKEN environment variable)."
            )
            return None
        if not self.config.slack_channel:
            self.println("Not requesting review: [slack] channel is not configured.")
            return None
        if self._cancel_requested():
            self.println(
                f"request_review for {self.metadata.rrid} was cancelled; "
                "nothing was done."
            )
            return None

        # 1) Auto-commit so the qam.suse.de /log mirror reflects the report we
        #    are asking people to review; abort before posting if it fails.
        #    (This ``svn ci`` commits the whole working copy, so it is also
        #    what re-commits a previously uncommitted local marker on a
        #    resume re-run.)
        try:
            svn_commit_testreport(
                self.metadata.report_wd(),
                self.config.install_logs,
                ["-m", "commit before review request"],
            )
        except subprocess.CalledProcessError as e:
            self.println(f"Failed to commit testreport, not requesting review: {e}")
            return None

        # 1b) The commit above ran ``svn up``, which may have pulled in a
        #     colleague's newer / reposted / removed marker — the file, not the
        #     load-time snapshot, decides whether to resume, and the in-memory
        #     attribute is synced so the approve/reject gate sees the same ts.
        existing = self.metadata.get_slack_review()
        self.metadata.slack_review = existing

        if existing is not None and not self.args.repost:
            # 2a) Resume the recorded request: no new post, no marker rewrite.
            #     The watch starts at the top of the thread, so replies that
            #     arrived while nobody was watching are forwarded first.
            channel, ts = existing
            if self.args.no_watch:
                self.println(
                    f"Review request for {self.metadata.rrid} already posted "
                    f"({channel}/{ts}); not watching. Pass --repost to post "
                    "a fresh one."
                )
                return None
            try:
                client = SlackClient(self.config)
            except SlackError as e:
                self.println(f"Slack client unavailable: {e}")
                return None
            self.println(
                f"Resuming existing review request for {self.metadata.rrid} "
                f"at {channel}/{ts} (pass --repost for a fresh one)."
            )
            return _PendingWatch(
                metadata=self.metadata,
                targets=self.targets,
                client=client,
                channel=channel,
                ts=ts,
                resumed=True,
                status="resumed, awaiting ack",
            )

        # 2b) Post a fresh request. A template that cannot record the review
        #     marker would leave a dangling Slack message whose ack the
        #     approve gate can never see; refuse before posting.
        if not self.metadata.has_slack_review_anchor():
            self.println(
                f"Testreport for {self.metadata.rrid} has no "
                "'Test Plan Reviewer:' line to anchor the Slack review "
                "marker; fix the template before requesting review."
            )
            return None

        url = self.metadata._testreport_url()  # noqa: SLF001 -- shared report URL helper
        if self._cancel_requested():
            self.println(
                f"request_review for {self.metadata.rrid} was cancelled; "
                "not posting the review request."
            )
            return None

        try:
            client = SlackClient(self.config)
            # The RRID must appear verbatim in this parent message: the
            # approve/reject review gate re-reads the posted message and
            # verifies its text names this RRID, defeating a forged marker.
            channel, ts = client.chat_postMessage(
                self.config.slack_channel,
                f"Please review {self.metadata.rrid}: {url}",
            )
        except SlackError as e:
            self.println(f"Failed to post Slack review request: {e}")
            return None

        # 3) Persist the marker so the ack survives a reload, then commit it.
        #    From here until the marker is recorded, the request is live but
        #    findable nowhere — any exit through this window must leave a
        #    breadcrumb (or void the message) instead of orphaning it.
        try:
            if self._cancel_requested():
                # Cancelled between post and persist: recording the marker
                # would resurrect a request the caller just cancelled, and not
                # recording it orphans a live message — void it while we still
                # hold the ts (best effort), like the unrecordable-marker path.
                logger.error(
                    "Cancelled with unrecorded Slack review request %s/%s",
                    channel,
                    ts,
                )
                with suppress(SlackError):
                    client.chat_postMessage(
                        channel,
                        f"This review request for {self.metadata.rrid} was "
                        "cancelled before it could be recorded and is void; "
                        "please ignore it.",
                        thread_ts=ts,
                    )
                self.println(
                    f"request_review for {self.metadata.rrid} was cancelled "
                    f"after posting {channel}/{ts}; the request was voided "
                    "and no marker was recorded."
                )
                return None
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
                return None
        except BaseException:
            # Ctrl-C (or any crash) between the post and the marker write
            # orphans a live request nobody can find later; leave a recovery
            # breadcrumb — best effort — then let the interrupt proceed.
            with suppress(Exception):
                logger.error(
                    "Interrupted with live, unrecorded Slack review request "
                    "%s/%s for %s; the message is live but unwatched — "
                    "re-run request_review (it posts or resumes a recorded "
                    "request) and disregard the orphaned message.",
                    channel,
                    ts,
                    self.metadata.rrid,
                )
            raise

        if self._cancel_requested():
            self.println(
                f"request_review for {self.metadata.rrid} was cancelled after "
                f"recording review request {channel}/{ts}; the marker was not "
                "committed to svn — a re-run of request_review commits it and "
                "resumes the watch."
            )
            return None
        try:
            svn_commit_testreport(
                self.metadata.report_wd(),
                self.config.install_logs,
                ["-m", f"Add Slack Review: {channel}/{ts}"],
            )
        except subprocess.CalledProcessError as e:
            # The cross-checkout durability of the review gate hinges on the
            # committed marker: watching (and possibly auto-approving) from a
            # marker that exists only in this checkout would break it. The
            # Slack post stays live and the marker is recorded locally; the
            # re-run's pre-commit (step 1) commits it and resumes the watch.
            logger.error("Failed to commit Slack review marker: %s", e)
            self.println(
                f"Posted review request {channel}/{ts} for "
                f"{self.metadata.rrid} but could not commit its marker to "
                f"svn: {e}. Not watching — other checkouts cannot see this "
                "review yet. The Slack post stays live and the marker is "
                "recorded locally; fix svn and re-run request_review to "
                "commit it and resume the watch."
            )
            return None

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
            self.println(f"Superseded previous review request {old_channel}/{old_ts}.")

        if self.args.no_watch:
            self.println(
                f"Posted review request for {self.metadata.rrid}; not watching."
            )
            return None

        return _PendingWatch(
            metadata=self.metadata,
            targets=self.targets,
            client=client,
            channel=channel,
            ts=ts,
            resumed=False,
            status="posted, awaiting ack",
        )

    def _watch_one(self, item: _PendingWatch) -> None:
        """Block on one request's thread until acked, timed out, or stopped.

        The classic single-template watch: delegates the polling to
        :meth:`SlackClient.wait_for_ack` and hands the outcome to
        :meth:`_conclude`. A review can take hours; the wait runs to
        ``slack_watch_timeout`` (a full working day by default). In the REPL
        press Ctrl-C to stop watching; over MCP call this with
        ``background=true`` and poll the job instead of blocking.
        """
        if self._cancel_requested():
            self.println(
                f"request_review for {item.metadata.rrid} was cancelled; "
                "not watching the review thread."
            )
            return
        self.println(
            f"Watching Slack for a 👍 on {item.metadata.rrid} "
            "(this can take a while; Ctrl-C to stop)."
        )
        with spinner(f"Waiting for review of {item.metadata.rrid}") as is_stopped:
            outcome = item.client.wait_for_ack(
                item.channel,
                item.ts,
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
        self._conclude(item, outcome)

    def _watch_all(self, watches: list[_PendingWatch]) -> None:
        """Run ONE combined watch over every pending request, round-robin.

        Polls each pending request in turn (reusing the per-message poll
        primitives of :class:`SlackClient`), concluding — and, unless
        ``--no-approve``, approving — each as its ack arrives, until every
        request resolves or the shared ``slack_watch_timeout`` deadline
        expires. The cancel event ends the watch promptly; the still-pending
        requests stay live and resumable.
        """
        if self._cancel_requested():
            self.println(
                "request_review was cancelled; not watching the posted review requests."
            )
            return
        rrids = ", ".join(str(item.metadata.rrid) for item in watches)
        self.println(
            f"Watching Slack for a 👍 on {rrids} "
            "(this can take a while; Ctrl-C to stop)."
        )
        interval = self.config.slack_poll_interval
        deadline = time.monotonic() + self.config.slack_watch_timeout
        cancel = current_cancel_event.get()
        sleeper = cancel or threading.Event()
        pending = list(watches)
        stop = False
        with spinner(f"Waiting for review of {rrids}") as is_stopped:
            while pending and not stop:
                for item in list(pending):
                    if is_stopped() or self._cancel_requested():
                        stop = True
                        break
                    outcome = self._poll_watch(item)
                    if outcome is not None:
                        pending.remove(item)
                        self._conclude(item, outcome)
                if stop or not pending or time.monotonic() >= deadline:
                    break
                # Interruptible sleep: stepped in ~0.1s slices so a stop
                # signal or a set cancel event takes effect within a tick
                # (mirrors ``SlackClient.wait_for_ack``).
                step = 0.1
                waited = 0.0
                while waited < interval and not is_stopped():
                    if cancel is not None and cancel.is_set():
                        break
                    sleeper.wait(min(step, interval - waited))
                    waited += step

        if not pending:
            return
        if self._cancel_requested():
            left = ", ".join(str(item.metadata.rrid) for item in pending)
            self.println(
                f"Watch cancelled with review requests still pending: {left}. "
                "Their markers are recorded; re-run request_review to resume."
            )
            return
        for item in pending:
            self._conclude(
                item,
                ReviewOutcome(
                    acked=False, reviewer=None, timed_out=True, unreachable=False
                ),
            )

    def _poll_watch(self, item: _PendingWatch) -> ReviewOutcome | None:
        """One poll cycle of one pending request in the combined watch.

        Streams new threaded replies (prefixed with the template's RRID, since
        several threads share the output) and checks the reactions for an ack,
        using the same per-message primitives ``wait_for_ack`` builds on.

        Returns:
            A :class:`ReviewOutcome` when the request resolved this cycle
            (acked, or unreachable after :data:`_MAX_POLL_FAILURES`
            consecutive failed cycles), or ``None`` while still pending.

        """
        try:
            messages = item.client.conversations_replies(item.channel, item.ts)
            for message in messages[item.last_seen :]:
                self.println(f"[{item.metadata.rrid}] {message.get('text', '')}")
                item.last_seen += 1
            reactions = item.client.reactions_get(item.channel, item.ts)
            reviewer = item.client._acking_reviewer(reactions)  # noqa: SLF001 -- shared ack primitive
            if reviewer:
                return ReviewOutcome(
                    acked=True, reviewer=reviewer, timed_out=False, unreachable=False
                )
            item.failures = 0
        except FailedSlackCallError as e:
            item.failures += 1
            logger.warning(
                "Slack poll for %s/%s failed (%s/%s consecutive): %s",
                item.channel,
                item.ts,
                item.failures,
                _MAX_POLL_FAILURES,
                e,
            )
            if item.failures >= _MAX_POLL_FAILURES:
                return ReviewOutcome(
                    acked=False, reviewer=None, timed_out=False, unreachable=True
                )
        return None

    def _conclude(self, item: _PendingWatch, outcome: ReviewOutcome) -> None:
        """Report one request's watch outcome and approve it on an ack."""
        channel, ts = item.channel, item.ts
        rrid = item.metadata.rrid
        if not outcome.acked:
            if outcome.unreachable and item.resumed:
                # On a resume this is the guaranteed outcome of a deleted
                # message, and every plain re-run resumes the same dead ts —
                # the remedy must be in the failure line itself.
                item.status = "watch failed (Slack unreachable or message deleted)"
                self.println(
                    f"Review watch for {rrid} failed (Slack "
                    f"unreachable, or the review message {channel}/{ts} was "
                    "deleted); re-run with --repost to post a fresh request."
                )
            elif outcome.unreachable:
                item.status = "watch failed (Slack unreachable)"
                self.println(f"Review watch for {rrid} failed (Slack unreachable)")
            else:
                item.status = "no ack (timed out or stopped)"
                self.println(f"No review ack for {rrid} (timed out or stopped)")
            return

        item.status = "acked"
        self.println(f"Review acked by {outcome.reviewer} (best-effort reactor)")
        if self.args.no_approve:
            item.status = "acked (--no-approve)"
            return
        self._approve_acked(item, outcome.reviewer)

    def _approve_acked(self, item: _PendingWatch, reviewer: str | None) -> None:
        """Drive ``approve`` for an acked request, guarding cancel/supersede.

        A cancelled job's watch may observe an ack in its final in-flight poll
        after ``job_cancel`` already returned — a cancelled watch must never
        mutate state. And the ack must still belong to the marker the report
        records: a ``--repost`` or a colleague's newer request supersedes this
        watch. The colleague's repost lives in the repository, not this
        checkout — the watch ran for hours since the last ``svn up`` — so
        refresh first (best-effort: an offline refresh falls back to the local
        state). The cancel event is re-checked immediately before the approve
        handoff, so a ``job_cancel`` landing during the refresh/re-read tail
        still refuses to approve.
        """
        metadata = item.metadata
        channel, ts = item.channel, item.ts
        if self._cancel_requested():
            item.status = "acked, not approved (job cancelled)"
            self.println(
                f"Watch for {metadata.rrid} was cancelled; the observed "
                "ack was not acted on."
            )
            return
        try:
            svn_update_testreport(metadata.report_wd())
        except (subprocess.CalledProcessError, OSError) as e:
            logger.warning("Could not refresh the testreport before approving: %s", e)
        if metadata.get_slack_review() != (channel, ts):
            item.status = "acked on a superseded request; not approved"
            self.println(
                f"Review request {channel}/{ts} for {metadata.rrid} was "
                "superseded; not approving from this watch."
            )
            return
        # Re-check as LATE as possible: the svn refresh and marker re-read
        # above take seconds, and a job cancelled in that window has already
        # been reported as cancelled — it must not approve anything.
        if self._cancel_requested():
            item.status = "acked, not approved (job cancelled)"
            self.println(
                f"Watch for {metadata.rrid} was cancelled; observed 👍 "
                "but not approving."
            )
            return

        # Approve THIS fanout template (not prompt's active one) by binding
        # the already-resolved metadata/targets onto a fresh Approve. Its
        # __call__ runs the Slack review gate, which passes because the ack
        # was just persisted and is still live.
        appr = Approve(
            Namespace(
                reviewer=reviewer,
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
        appr.metadata = metadata
        appr.targets = item.targets
        try:
            appr.__call__()
        except FailedSlackCallError as e:
            # The gate's own live re-query of the ack can hiccup right after
            # an hours-long watch; the state stays fail-closed (not approved)
            # and the remedy must be actionable, not a raw traceback.
            item.status = "acked; approve failed (Slack error)"
            self.println(
                f"Observed 👍 for {metadata.rrid}, but the approve step "
                f"failed talking to Slack: {e}. The update was NOT approved; "
                "re-run request_review to resume the watch, or run approve "
                "manually once Slack settles."
            )
            return
        item.status = "approved"

    def _report_interrupted(
        self, watches: list[_PendingWatch], unprocessed: list[str]
    ) -> None:
        """Print the per-template status summary after a Ctrl-C.

        Every posted request and its marker survive the interrupt; pending
        ones resume on a re-run via their recorded markers, so the summary
        says where each template stands instead of losing them silently.
        """
        self.println("request_review interrupted; per-template status:")
        for item in watches:
            self.println(f"  {item.metadata.rrid}: {item.status}")
        for rrid in unprocessed:
            self.println(
                f"  {rrid}: interrupted before its request was posted "
                "(re-run request_review to post or resume it)"
            )
        if watches:
            self.println(
                "Posted requests stay live and their markers are recorded; "
                "re-run request_review to resume watching the pending ones."
            )

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
