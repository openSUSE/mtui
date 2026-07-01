"""Tests for the ``request_review`` command.

Covers the commit-before-post ordering, the Slack post + durable marker
persistence (the marker records the canonical channel Slack returns, not the
configured alias), the ``--no-watch`` early return, the not-loaded guard, the
config and cooperative-cancel guards before every side effect, the two-phase
multi-template fan-out (all posts before any watch; Ctrl-C keeps the other
templates' posts and reports per-template status), and the blocker-fix
regression: the auto-approve after a 👍 must act on the fanout iteration's
template, not the prompt's active one.
"""

from __future__ import annotations

import logging
import subprocess
import threading
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.request_review import RequestReview
from mtui.data_sources.slack import ReviewOutcome
from mtui.support.cancellation import current_cancel_event
from mtui.support.exceptions import FailedSlackCallError
from mtui.support.messages import FanOutError, TestReportNotLoadedError
from mtui.test_reports.svn_io import TemplateFormatError


def _outcome(*, acked=True, reviewer="alice", timed_out=False, unreachable=False):
    """Build a ReviewOutcome the patched ``wait_for_ack`` returns."""
    return ReviewOutcome(
        acked=acked,
        reviewer=reviewer,
        timed_out=timed_out,
        unreachable=unreachable,
    )


def _wire_marker(metadata: MagicMock, initial=None) -> dict:
    """Make the metadata mock's marker behave like the real file-backed one.

    ``get_slack_review`` reflects the last ``set_slack_review`` write (the
    command re-reads the on-disk marker for its resume decision and for the
    pre-approve supersede guard). Returns the state dict so a test can mutate
    the marker mid-watch to simulate a concurrent repost.
    """
    state = {"marker": initial}
    metadata.set_slack_review.side_effect = lambda c, t: state.__setitem__(
        "marker", (c, t)
    )
    metadata.get_slack_review.side_effect = lambda: state["marker"]
    metadata.slack_review = initial
    return state


def _printed(sys: MagicMock) -> str:
    """Join everything the command wrote via ``println`` into one string."""
    return "".join(c.args[0] for c in sys.stdout.write.call_args_list)


def _prompt() -> MagicMock:
    """A prompt whose active metadata is a loaded, distinct report."""
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.rrid = "SUSE:Maintenance:1:1"
    p.metadata.report_wd.return_value = "/wd"
    p.metadata._testreport_url.return_value = "http://qam/1/log"
    _wire_marker(p.metadata)
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def _report(rrid: str, url: str) -> MagicMock:
    """A loaded fan-out report mock with a wired file-backed marker."""
    r = MagicMock(name=f"report_{rrid}")
    r.__bool__ = lambda self: True
    r.id = rrid
    r.rrid = rrid
    r.report_wd.return_value = "/wd"
    r._testreport_url.return_value = url
    r.targets = MagicMock(name=f"targets_{rrid}")
    _wire_marker(r)
    return r


def _args(**overrides) -> Namespace:
    base = {
        "no_watch": False,
        "no_approve": False,
        "repost": False,
        "group": ["qam-sle"],
        "user": "",
        "template": None,
        "all_templates": False,
    }
    base.update(overrides)
    return Namespace(**base)


#: What the (mocked, per the SlackClient contract) ``chat_postMessage``
#: returns: the CANONICAL channel id from Slack's response — deliberately
#: different from ``mock_config.slack_channel`` ("C123") so the tests prove
#: the marker persists the returned channel, not the configured alias.
CANON = "C123CANON"


# ---------------------------------------------------------------------------
# not loaded
# ---------------------------------------------------------------------------


def test_request_review_without_metadata_raises(mock_config):
    prompt = MagicMock()
    prompt.metadata = MagicMock()
    prompt.metadata.__bool__ = lambda self: False
    prompt.display = MagicMock()
    prompt.targets = MagicMock()

    with pytest.raises(TestReportNotLoadedError):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()


# ---------------------------------------------------------------------------
# happy path: commit BEFORE post, URL posted, marker persisted with the
# canonical channel + ts that Slack returned
# ---------------------------------------------------------------------------


def test_request_review_commits_before_posting_and_persists(mock_config):
    prompt = _prompt()
    order: list[str] = []

    client = MagicMock()

    def _post(channel, text):
        order.append("post")
        return (CANON, "1700000000.000100")

    client.chat_postMessage.side_effect = _post
    client.wait_for_ack.return_value = _outcome()

    def _commit(*a, **kw):
        order.append("commit")

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport", side_effect=_commit
        ),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    # The first svn commit MUST precede the Slack post (the /log mirror only
    # reflects committed content).
    assert order[0] == "commit"
    assert order.index("commit") < order.index("post")

    # The report URL was posted to the CONFIGURED channel, and the parent
    # message names the RRID verbatim: the approve/reject gate re-reads the
    # posted text and verifies it contains this RRID (anti-forgery contract).
    posted_channel, posted_text = client.chat_postMessage.call_args.args
    assert posted_channel == mock_config.slack_channel
    assert "http://qam/1/log" in posted_text
    assert str(prompt.metadata.rrid) in posted_text

    # The marker was persisted with the canonical channel + ts Slack
    # RETURNED, not the configured channel alias.
    prompt.metadata.set_slack_review.assert_called_once_with(CANON, "1700000000.000100")


def test_request_review_aborts_before_posting_when_commit_fails(mock_config):
    """A failed pre-commit must not post to Slack nor persist a marker."""
    prompt = _prompt()
    client = MagicMock()

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport",
            side_effect=subprocess.CalledProcessError(1, "svn"),
        ),
        patch(
            "mtui.commands.request_review.SlackClient", return_value=client
        ) as slack_cls,
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    slack_cls.assert_not_called()
    client.chat_postMessage.assert_not_called()
    prompt.metadata.set_slack_review.assert_not_called()
    appr_cls.assert_not_called()


# ---------------------------------------------------------------------------
# config guards: unset [slack] token / channel refuse up front, clearly
# ---------------------------------------------------------------------------


def test_request_review_missing_slack_token_refuses_up_front(mock_config):
    """An empty [slack] token errors clearly before ANY side effect."""
    mock_config.slack_token = ""
    prompt = _prompt()
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport") as commit,
        patch("mtui.commands.request_review.SlackClient") as slack_cls,
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    commit.assert_not_called()
    slack_cls.assert_not_called()
    appr_cls.assert_not_called()
    assert "[slack] token" in _printed(sys)


def test_request_review_missing_slack_channel_refuses_up_front(mock_config):
    """An empty [slack] channel errors clearly instead of a live API call
    dying with a misleading channel_not_found."""
    mock_config.slack_channel = ""
    prompt = _prompt()
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport") as commit,
        patch("mtui.commands.request_review.SlackClient") as slack_cls,
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    commit.assert_not_called()
    slack_cls.assert_not_called()
    appr_cls.assert_not_called()
    assert "[slack] channel" in _printed(sys)


# ---------------------------------------------------------------------------
# --no-watch returns early without watching
# ---------------------------------------------------------------------------


def test_request_review_no_watch_returns_before_watching(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000200")

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(no_watch=True), mock_config, MagicMock(), prompt)()

    client.chat_postMessage.assert_called_once()
    prompt.metadata.set_slack_review.assert_called_once_with(CANON, "1700000000.000200")
    # Early return: never watched, never approved.
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()


# ---------------------------------------------------------------------------
# --no-approve: watches + reports the ack but does not approve
# ---------------------------------------------------------------------------


def test_request_review_no_approve_watches_but_skips_approve(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000300")
    client.wait_for_ack.return_value = _outcome(reviewer="bob")

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(no_approve=True), mock_config, MagicMock(), prompt)()

    client.wait_for_ack.assert_called_once()
    appr_cls.assert_not_called()


def test_request_review_not_acked_does_not_approve(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000400")
    client.wait_for_ack.return_value = _outcome(
        acked=False, reviewer=None, timed_out=True
    )

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    appr_cls.assert_not_called()


# ---------------------------------------------------------------------------
# happy path: on ack, drive Approve bound to the reactor as reviewer
# ---------------------------------------------------------------------------


def test_request_review_acked_drives_approve_with_reviewer(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000500")
    client.wait_for_ack.return_value = _outcome(reviewer="carol")

    appr = MagicMock(name="Approve")

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve", return_value=appr) as appr_cls,
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    appr_cls.assert_called_once()
    passed_args = appr_cls.call_args.args[0]
    assert passed_args.reviewer == "carol"
    # The best-effort reactor drives the internal approve; it must run
    # (``appr.__call__()`` marks the mock as called).
    assert appr.called
    # Bound to the loaded report, not left unset.
    assert appr.metadata is prompt.metadata


def test_request_review_approve_slack_hiccup_reports_actionably(mock_config):
    """A FailedSlackCallError in the approve handoff after a successful ack
    must not crash the hours-long watch: the state stays fail-closed and the
    message tells the user how to recover."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000550")
    client.wait_for_ack.return_value = _outcome(reviewer="carol")
    appr = MagicMock(name="Approve")
    appr.side_effect = FailedSlackCallError("internal_error")
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve", return_value=appr),
    ):
        # Must not raise despite the approve handoff failing.
        RequestReview(_args(), mock_config, sys, prompt)()

    printed = _printed(sys)
    assert "NOT approved" in printed
    assert "re-run request_review" in printed
    assert "internal_error" in printed


# ---------------------------------------------------------------------------
# BLOCKER regression: auto-approve after 👍 acts on the intended fanout
# template, not the prompt's active one. The fan-out now runs two-phase
# (posts first, then ONE combined watch), so the ack is observed by the
# round-robin poll, not a per-template wait_for_ack.
# ---------------------------------------------------------------------------


def test_autoapprove_targets_fanout_template_not_active(mock_config):
    """Ack on the non-active report must approve *that* report's metadata.

    With several templates loaded, the fan-out posts every request and then
    watches them all in one combined loop. The blocker was a bare
    ``Approve()`` reading the prompt's *active* metadata instead of the
    fanout iteration's target — a 👍 on report B could approve report A. This
    proves the constructed ``Approve`` is bound to the acked report's
    metadata/targets, not the active one.
    """
    prompt = _prompt()

    # The active template (what a buggy implementation would wrongly approve).
    active_meta = prompt.metadata

    # Two distinct loaded reports; ack lands on the NON-active one.
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]

    # One poll round is enough: B acks immediately, A runs out the deadline.
    mock_config.slack_watch_timeout = 0

    client = MagicMock()
    client.chat_postMessage.side_effect = [(CANON, "111.a"), (CANON, "222.b")]
    client.conversations_replies.return_value = [{"text": "parent"}]
    client.reactions_get.side_effect = lambda channel, ts: (
        [{"name": "+1", "users": ["U1"]}] if ts == "222.b" else []
    )
    client._acking_reviewer.side_effect = lambda reactions: (
        "dave" if reactions else None
    )

    built: list[MagicMock] = []

    def _approve_factory(args, config, sys, prompt_):
        appr = MagicMock(name="Approve")
        built.append(appr)
        return appr

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve", side_effect=_approve_factory),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt).run()

    # Exactly one approve was constructed + fired (the acked report B).
    fired = [a for a in built if a.called]
    assert len(fired) == 1
    approved = fired[0]
    assert approved.metadata is report_b
    assert approved.targets is report_b.targets
    # And it was NOT the prompt's active metadata (the blocker symptom).
    assert approved.metadata is not active_meta


# ---------------------------------------------------------------------------
# two-phase fan-out: every template's request is POSTED before any watch,
# and Ctrl-C mid-watch keeps the other templates' posts + markers and
# reports a per-template status summary.
# ---------------------------------------------------------------------------


def test_fanout_posts_all_templates_before_any_watch(mock_config):
    """No template's post may wait behind another template's watch."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]
    mock_config.slack_watch_timeout = 0

    order: list[str] = []
    client = MagicMock()

    def _post(channel, text, thread_ts=None):
        order.append("post")
        return (CANON, f"{len(order)}.0")

    def _replies(channel, ts):
        order.append("poll")
        return [{"text": "parent"}]

    client.chat_postMessage.side_effect = _post
    client.conversations_replies.side_effect = _replies
    client.reactions_get.return_value = []
    client._acking_reviewer.return_value = None

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt).run()

    # Both posts happened, and every post preceded the first poll.
    assert order.count("post") == 2
    assert "poll" in order
    assert max(i for i, o in enumerate(order) if o == "post") < order.index("poll")
    # Both markers were persisted (with the canonical returned channel).
    report_a.set_slack_review.assert_called_once_with(CANON, "1.0")
    report_b.set_slack_review.assert_called_once_with(CANON, "2.0")


def test_fanout_keyboardinterrupt_mid_watch_reports_status(mock_config):
    """Ctrl-C in the combined watch must not lose the other templates.

    Both requests were already posted and their markers recorded; the
    interrupt stops the watch, prints a per-template status summary, and
    does not propagate (pending requests resume on a re-run).
    """
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]

    client = MagicMock()
    client.chat_postMessage.side_effect = [(CANON, "111.a"), (CANON, "222.b")]
    client.conversations_replies.side_effect = KeyboardInterrupt
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        # Must not raise: run() converts the interrupt into a status report.
        RequestReview(_args(), mock_config, sys, prompt).run()

    # Both posts and both markers survived the interrupt.
    assert client.chat_postMessage.call_count == 2
    report_a.set_slack_review.assert_called_once_with(CANON, "111.a")
    report_b.set_slack_review.assert_called_once_with(CANON, "222.b")
    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "interrupted" in printed
    assert "SUSE:Maintenance:1:1: posted, awaiting ack" in printed
    assert "SUSE:Maintenance:2:2: posted, awaiting ack" in printed
    assert "re-run request_review" in printed


def test_fanout_scoped_single_template_keeps_blocking_watch(mock_config):
    """A ``-T``-scoped invocation (what the MCP session mints per background
    job) behaves exactly like the classic single-template flow: one blocking
    ``wait_for_ack``, no round-robin polling."""
    prompt = _prompt()
    report = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.get.return_value = report

    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "222.b")
    client.wait_for_ack.return_value = _outcome(
        acked=False, reviewer=None, timed_out=True
    )

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(
            _args(template="SUSE:Maintenance:2:2"), mock_config, MagicMock(), prompt
        ).run()

    client.wait_for_ack.assert_called_once()
    client.conversations_replies.assert_not_called()


def test_fanout_phase1_failure_collected_others_still_watched(mock_config):
    """One template's unexpected phase-1 failure must not stop the others:
    the failure is collected (and raised as FanOutError afterwards) while the
    other template's request is still posted AND watched."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    report_a.get_slack_review.side_effect = RuntimeError("boom")
    prompt.templates.all.return_value = [report_a, report_b]
    mock_config.slack_watch_timeout = 0

    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "222.b")
    client.conversations_replies.return_value = [{"text": "parent"}]
    client.reactions_get.return_value = []
    client._acking_reviewer.return_value = None

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
        pytest.raises(FanOutError),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt).run()

    # B was posted, its marker persisted, and its watch polled despite A.
    report_b.set_slack_review.assert_called_once_with(CANON, "222.b")
    client.conversations_replies.assert_called()


def test_fanout_cancel_before_combined_watch_skips_it(mock_config):
    """A cancel landing after the last post stops before the combined watch;
    both markers survive for a later resume."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]

    cancel = threading.Event()
    commits: list[int] = []

    def _commit(*a, **kw):
        commits.append(1)
        if len(commits) == 4:  # B's marker commit — the last phase-1 commit
            cancel.set()

    client = MagicMock()
    client.chat_postMessage.side_effect = [(CANON, "111.a"), (CANON, "222.b")]
    sys = MagicMock()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch(
                "mtui.commands.request_review.svn_commit_testreport",
                side_effect=_commit,
            ),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt).run()
    finally:
        current_cancel_event.reset(token)

    report_a.set_slack_review.assert_called_once_with(CANON, "111.a")
    report_b.set_slack_review.assert_called_once_with(CANON, "222.b")
    client.conversations_replies.assert_not_called()
    appr_cls.assert_not_called()
    assert "not watching the posted review requests" in _printed(sys)


def test_fanout_cancel_mid_combined_watch_reports_pending(mock_config):
    """A cancel during the combined watch stops the round-robin promptly and
    names the still-pending requests (their markers stay resumable)."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]

    cancel = threading.Event()
    client = MagicMock()
    client.chat_postMessage.side_effect = [(CANON, "111.a"), (CANON, "222.b")]

    def _replies(channel, ts):
        # job_cancel lands while template A's poll is in flight.
        cancel.set()
        return [{"text": "parent"}]

    client.conversations_replies.side_effect = _replies
    client.reactions_get.return_value = []
    client._acking_reviewer.return_value = None
    sys = MagicMock()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport"),
            patch("mtui.commands.request_review.svn_update_testreport"),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt).run()
    finally:
        current_cancel_event.reset(token)

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "Watch cancelled with review requests still pending" in printed
    assert "SUSE:Maintenance:1:1" in printed
    assert "SUSE:Maintenance:2:2" in printed


def test_fanout_unreachable_template_reported_after_max_failures(mock_config):
    """Consecutive poll failures mark ONE pending request unreachable, with
    the same degradation as the single-template wait_for_ack."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]
    # Enough deadline for three poll rounds; no sleeping between them.
    mock_config.slack_watch_timeout = 30
    mock_config.slack_poll_interval = 0

    client = MagicMock()
    client.chat_postMessage.side_effect = [(CANON, "111.a"), (CANON, "222.b")]
    client.conversations_replies.side_effect = FailedSlackCallError("boom")
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt).run()

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "Review watch for SUSE:Maintenance:1:1 failed (Slack unreachable)" in printed
    assert "Review watch for SUSE:Maintenance:2:2 failed (Slack unreachable)" in printed


def test_fanout_keyboardinterrupt_in_phase1_reports_unposted(mock_config):
    """Ctrl-C during phase 1 reports the templates that never got posted, so
    nothing is dropped silently."""
    prompt = _prompt()
    report_a = _report("SUSE:Maintenance:1:1", "http://qam/1/log")
    report_b = _report("SUSE:Maintenance:2:2", "http://qam/2/log")
    prompt.templates.all.return_value = [report_a, report_b]

    commits: list[int] = []

    def _commit(*a, **kw):
        commits.append(1)
        if len(commits) == 3:  # B's pre-commit: interrupt before its post
            raise KeyboardInterrupt

    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "111.a")
    sys = MagicMock()

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport",
            side_effect=_commit,
        ),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        # Must not raise: run() converts the interrupt into a status report.
        RequestReview(_args(), mock_config, sys, prompt).run()

    report_a.set_slack_review.assert_called_once_with(CANON, "111.a")
    report_b.set_slack_review.assert_not_called()
    printed = _printed(sys)
    assert "SUSE:Maintenance:1:1: posted, awaiting ack" in printed
    assert "SUSE:Maintenance:2:2: interrupted before its request was posted" in printed


# ---------------------------------------------------------------------------
# failure paths: Slack post fails, marker commit fails, Slack unreachable
# ---------------------------------------------------------------------------


def test_request_review_slack_post_failure_aborts(mock_config):
    """A failed Slack post reports the error and neither persists nor watches."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.side_effect = FailedSlackCallError("channel_not_found")
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    prompt.metadata.set_slack_review.assert_not_called()
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "Failed to post Slack review request" in printed


def test_request_review_marker_commit_failure_aborts_before_watch(mock_config):
    """A failed marker commit ABORTS before the watch: a marker that exists
    nowhere but this checkout must never feed an auto-approve. The Slack post
    stays live; a re-run's pre-commit re-commits the local marker and resumes."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000700")
    sys = MagicMock()

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport",
            side_effect=[None, subprocess.CalledProcessError(1, "svn")],
        ),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    # The marker was persisted locally, but the flow stopped before the watch.
    prompt.metadata.set_slack_review.assert_called_once_with(CANON, "1700000000.000700")
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "could not commit its marker" in printed
    assert "re-run request_review" in printed


def test_request_review_unreachable_slack_reported(mock_config):
    """An unreachable-Slack outcome is reported distinctly and skips approve."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000800")
    client.wait_for_ack.return_value = _outcome(
        acked=False, reviewer=None, unreachable=True
    )
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "Slack unreachable" in printed


def test_request_review_missing_anchor_aborts_before_posting(mock_config):
    """A template that cannot record the marker refuses before posting."""
    prompt = _prompt()
    prompt.metadata.has_slack_review_anchor.return_value = False
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient") as slack_cls,
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    # Nothing was posted: no client, no marker, no watch, no approve.
    slack_cls.assert_not_called()
    prompt.metadata.set_slack_review.assert_not_called()
    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "no 'Test Plan Reviewer:' line" in printed


def test_request_review_marker_write_failure_reports_and_stops(mock_config):
    """A marker write failing post-post is reported; the watch is skipped."""
    prompt = _prompt()
    prompt.metadata.set_slack_review.side_effect = TemplateFormatError("no anchor")
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000900")
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    # Watching would be pointless: the approve gate can never see the review.
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "could not record the marker" in printed
    # A plain re-run would RESUME the stale previous marker; the remedy must
    # name --repost and the orphaned ts, and be executable in a multi-template
    # session (where unscoped --repost is refused).
    assert "--repost" in printed
    assert "1700000000.000900" in printed
    assert "-T SUSE:Maintenance:1:1" in printed
    # The unrecorded message would otherwise collect acks nobody watches: it
    # gets a best-effort "void" reply threaded under it, while the ts is hot.
    post, void = client.chat_postMessage.call_args_list
    assert void.kwargs["thread_ts"] == "1700000000.000900"
    assert "void" in void.args[1]


def test_request_review_interrupt_between_post_and_persist_breadcrumbs(
    mock_config, caplog
):
    """Ctrl-C between the post and the marker write logs a recovery
    breadcrumb (channel/ts) for the live, unrecorded request — then the
    interrupt still propagates."""
    prompt = _prompt()
    prompt.metadata.set_slack_review.side_effect = KeyboardInterrupt
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.000950")

    caplog.set_level(logging.ERROR, logger="mtui.command.request_review")
    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
        pytest.raises(KeyboardInterrupt),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    assert "unrecorded Slack review request" in caplog.text
    assert CANON in caplog.text
    assert "1700000000.000950" in caplog.text


def test_request_review_forwards_mcp_cancel_event(mock_config):
    """The MCP session's per-call cancel event reaches wait_for_ack."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.001000")
    client.wait_for_ack.return_value = _outcome(
        acked=False, reviewer=None, timed_out=True
    )

    cancel = threading.Event()
    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport"),
            patch("mtui.commands.request_review.svn_update_testreport"),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve"),
        ):
            RequestReview(_args(), mock_config, MagicMock(), prompt)()
    finally:
        current_cancel_event.reset(token)

    assert client.wait_for_ack.call_args.kwargs["cancel_event"] is cancel


def test_request_review_complete_offers_flags_and_templates(mock_config):
    """Completion offers the command's flags plus the loaded template RRIDs."""
    templates = MagicMock()
    templates.rrids.return_value = ["SUSE:Maintenance:1:1"]

    out = RequestReview.complete(
        {"templates": templates}, "", "request_review ", 15, 15
    )

    assert "--no-watch" in out
    assert "--no-approve" in out
    assert "--repost" in out
    assert "SUSE:Maintenance:1:1" in out


# ---------------------------------------------------------------------------
# cooperative cancellation: the cancel event is honoured BEFORE every
# externally visible side effect, not just around the watch
# ---------------------------------------------------------------------------


def test_request_review_cancelled_before_start_does_nothing(mock_config):
    """A job cancelled before the command body runs makes NO side effect:
    no svn commit, no Slack client, no post, no marker."""
    prompt = _prompt()
    sys = MagicMock()
    cancel = threading.Event()
    cancel.set()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport") as commit,
            patch("mtui.commands.request_review.SlackClient") as slack_cls,
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    commit.assert_not_called()
    slack_cls.assert_not_called()
    prompt.metadata.set_slack_review.assert_not_called()
    appr_cls.assert_not_called()
    assert "cancelled" in _printed(sys)


def test_request_review_cancelled_after_precommit_skips_post(mock_config):
    """A cancel landing during the pre-commit stops before the Slack post."""
    prompt = _prompt()
    sys = MagicMock()
    cancel = threading.Event()

    def _commit(*a, **kw):
        cancel.set()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch(
                "mtui.commands.request_review.svn_commit_testreport",
                side_effect=_commit,
            ),
            patch("mtui.commands.request_review.SlackClient") as slack_cls,
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    slack_cls.assert_not_called()
    prompt.metadata.set_slack_review.assert_not_called()
    appr_cls.assert_not_called()
    assert "not posting" in _printed(sys)


def test_request_review_cancelled_between_post_and_persist_voids(mock_config):
    """A cancel landing during the post voids the just-posted message (best
    effort) instead of recording a marker for a cancelled job."""
    prompt = _prompt()
    sys = MagicMock()
    cancel = threading.Event()
    client = MagicMock()

    def _post(channel, text, thread_ts=None):
        if thread_ts is None:
            cancel.set()
            return (CANON, "555.5")
        return (CANON, "555.6")

    client.chat_postMessage.side_effect = _post

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport"),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    prompt.metadata.set_slack_review.assert_not_called()
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    # The live message got a best-effort "void" reply threaded under it.
    post, void = client.chat_postMessage.call_args_list
    assert void.kwargs["thread_ts"] == "555.5"
    assert "void" in void.args[1]
    assert "voided" in _printed(sys)


def test_request_review_cancelled_before_marker_commit_stops(mock_config):
    """A cancel landing during the marker write stops before the svn commit
    of the marker; the local marker makes a re-run resume + commit it."""
    prompt = _prompt()
    sys = MagicMock()
    cancel = threading.Event()
    state = {"marker": None}

    def _set(c, t):
        state["marker"] = (c, t)
        cancel.set()

    prompt.metadata.set_slack_review.side_effect = _set
    prompt.metadata.get_slack_review.side_effect = lambda: state["marker"]
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "666.6")

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport") as commit,
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    # Only the pre-commit ran; the marker commit was refused post-cancel.
    assert commit.call_count == 1
    assert state["marker"] == (CANON, "666.6")
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    assert "not committed" in _printed(sys)


def test_request_review_cancelled_before_watch_never_watches(mock_config):
    """A cancel landing during the marker commit stops before the watch."""
    prompt = _prompt()
    sys = MagicMock()
    cancel = threading.Event()
    commits: list[int] = []

    def _commit(*a, **kw):
        commits.append(1)
        if len(commits) == 2:  # the marker commit, after the pre-commit
            cancel.set()

    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "777.1")

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch(
                "mtui.commands.request_review.svn_commit_testreport",
                side_effect=_commit,
            ),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()
    assert "not watching" in _printed(sys)


# ---------------------------------------------------------------------------
# resume: a recorded marker is watched instead of re-posted
# ---------------------------------------------------------------------------


def test_request_review_resumes_existing_marker(mock_config):
    """A recorded marker resumes that thread: no post, no marker rewrite.

    The watch runs on the MARKER's channel/ts (not the configured channel),
    and an ack on the resumed thread still drives the auto-approve.
    """
    prompt = _prompt()
    _wire_marker(prompt.metadata, initial=("C_OLD", "111.1"))
    client = MagicMock()
    client.wait_for_ack.return_value = _outcome(reviewer="carol")
    appr = MagicMock(name="Approve")

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve", return_value=appr),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    client.chat_postMessage.assert_not_called()
    prompt.metadata.set_slack_review.assert_not_called()
    watched_channel, watched_ts = client.wait_for_ack.call_args.args[:2]
    assert (watched_channel, watched_ts) == ("C_OLD", "111.1")
    assert appr.called


def test_request_review_resume_no_watch_wording(mock_config):
    """Resume + --no-watch reports the existing request without 'Posted'."""
    prompt = _prompt()
    _wire_marker(prompt.metadata, initial=("C_OLD", "111.1"))
    client = MagicMock()
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(no_watch=True), mock_config, sys, prompt)()

    client.chat_postMessage.assert_not_called()
    client.wait_for_ack.assert_not_called()
    printed = _printed(sys)
    assert "already posted (C_OLD/111.1)" in printed
    assert "SUSE:Maintenance:1:1" in printed
    assert "Posted review request" not in printed


def test_request_review_repost_replaces_marker_and_breadcrumbs(mock_config):
    """--repost posts fresh, replaces the marker, and marks the old thread."""
    prompt = _prompt()
    state = _wire_marker(prompt.metadata, initial=("C_OLD", "111.1"))
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "222.2")
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(repost=True, no_watch=True), mock_config, sys, prompt)()

    # First post is the fresh request to the CONFIGURED channel; the second is
    # the best-effort "superseded" breadcrumb threaded under the OLD message.
    first, second = client.chat_postMessage.call_args_list
    assert first.args[0] == mock_config.slack_channel
    assert "Please review" in first.args[1]
    assert second.args[0] == "C_OLD"
    assert second.kwargs["thread_ts"] == "111.1"
    assert "Superseded" in second.args[1]
    # The marker now records the new request under its CANONICAL channel.
    assert state["marker"] == (CANON, "222.2")
    printed = _printed(sys)
    assert "Superseded previous review request C_OLD/111.1" in printed


def test_request_review_repost_multi_template_refused(mock_config):
    """--repost over an unscoped multi-template fan-out is refused up front."""
    prompt = _prompt()
    prompt.templates.all.return_value = [MagicMock(), MagicMock()]
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport") as commit,
        patch("mtui.commands.request_review.SlackClient") as slack_cls,
    ):
        # The guard lives in run() so it fires once, before the fan-out.
        RequestReview(_args(repost=True), mock_config, sys, prompt).run()

    commit.assert_not_called()
    slack_cls.assert_not_called()
    printed = _printed(sys)
    # Printed exactly once, not once per template.
    assert printed.count("scope it with -T") == 1


def test_request_review_resume_slack_client_unavailable(mock_config):
    """A Slack setup failure on the resume path is reported, not raised."""
    prompt = _prompt()
    _wire_marker(prompt.metadata, initial=("C_OLD", "111.1"))
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch(
            "mtui.commands.request_review.SlackClient",
            side_effect=FailedSlackCallError("no token"),
        ),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    appr_cls.assert_not_called()
    assert "Slack client unavailable" in _printed(sys)


def test_request_review_resume_unreachable_hints_repost(mock_config):
    """An unreachable outcome on a RESUMED watch points at --repost."""
    prompt = _prompt()
    _wire_marker(prompt.metadata, initial=("C_OLD", "111.1"))
    client = MagicMock()
    client.wait_for_ack.return_value = _outcome(
        acked=False, reviewer=None, unreachable=True
    )
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "re-run with --repost" in printed
    assert "C_OLD/111.1" in printed


# ---------------------------------------------------------------------------
# pre-approve guards: cancelled watch / superseded marker never approve
# ---------------------------------------------------------------------------


def test_request_review_cancelled_watch_never_approves(mock_config):
    """An ack observed by an already-cancelled watch is not acted on."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.001100")

    cancel = threading.Event()

    def _ack_after_cancel(*a, **kw):
        # The 👍 lands in the final in-flight poll after job_cancel already
        # flagged the event — the race the guard exists for.
        cancel.set()
        return _outcome(reviewer="late")

    client.wait_for_ack.side_effect = _ack_after_cancel
    sys = MagicMock()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport"),
            patch("mtui.commands.request_review.svn_update_testreport"),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "cancelled" in printed


def test_request_review_cancel_during_preapprove_tail_refuses(mock_config):
    """TOCTOU regression: a cancel landing DURING the post-ack tail (svn
    refresh / marker re-read) must still refuse the approve — the event is
    re-checked immediately before the Approve handoff."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "1700000000.001150")
    client.wait_for_ack.return_value = _outcome(reviewer="eve")
    sys = MagicMock()
    cancel = threading.Event()

    def _svn_up(wd):
        # job_cancel returns while the multi-second tail runs unguarded —
        # the window the late re-check closes.
        cancel.set()

    token = current_cancel_event.set(cancel)
    try:
        with (
            patch("mtui.commands.request_review.svn_commit_testreport"),
            patch(
                "mtui.commands.request_review.svn_update_testreport",
                side_effect=_svn_up,
            ),
            patch("mtui.commands.request_review.SlackClient", return_value=client),
            patch("mtui.commands.request_review.Approve") as appr_cls,
        ):
            RequestReview(_args(), mock_config, sys, prompt)()
    finally:
        current_cancel_event.reset(token)

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "not approving" in printed


def test_request_review_superseded_marker_never_approves(mock_config):
    """An ack on a thread the report no longer records is not acted on."""
    prompt = _prompt()
    state = _wire_marker(prompt.metadata)
    client = MagicMock()
    client.chat_postMessage.return_value = (CANON, "333.3")

    def _ack_after_supersede(*a, **kw):
        # A colleague reposted while we watched: the on-disk marker moved on.
        state["marker"] = ("C_NEW", "999.9")
        return _outcome(reviewer="late")

    client.wait_for_ack.side_effect = _ack_after_supersede
    sys = MagicMock()

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(), mock_config, sys, prompt)()

    appr_cls.assert_not_called()
    printed = _printed(sys)
    assert "superseded" in printed
