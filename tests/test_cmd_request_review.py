"""Tests for the ``request_review`` command.

Covers the commit-before-post ordering, the Slack post + durable marker
persistence, the ``--no-watch`` early return, the not-loaded guard, and the
blocker-fix regression: the auto-approve after a 👍 must act on the fanout
iteration's template, not the prompt's active one.
"""

from __future__ import annotations

import subprocess
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.request_review import RequestReview
from mtui.data_sources.slack import ReviewOutcome
from mtui.support.cancellation import current_cancel_event
from mtui.support.exceptions import FailedSlackCallError
from mtui.support.messages import TestReportNotLoadedError
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
# happy path: commit BEFORE post, URL posted, marker persisted with the ts
# ---------------------------------------------------------------------------


def test_request_review_commits_before_posting_and_persists(mock_config):
    prompt = _prompt()
    order: list[str] = []

    client = MagicMock()

    def _post(channel, text):
        order.append("post")
        return "1700000000.000100"

    client.chat_postMessage.side_effect = _post
    client.wait_for_ack.return_value = _outcome()

    def _commit(*a, **kw):
        order.append("commit")

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport", side_effect=_commit
        ),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(), mock_config, MagicMock(), prompt)()

    # The first svn commit MUST precede the Slack post (the /log mirror only
    # reflects committed content).
    assert order[0] == "commit"
    assert order.index("commit") < order.index("post")

    # The report URL was posted.
    posted_channel, posted_text = client.chat_postMessage.call_args.args
    assert posted_channel == mock_config.slack_channel
    assert "http://qam/1/log" in posted_text

    # The marker was persisted with the returned ts.
    prompt.metadata.set_slack_review.assert_called_once_with(
        mock_config.slack_channel, "1700000000.000100"
    )


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
# --no-watch returns early without watching
# ---------------------------------------------------------------------------


def test_request_review_no_watch_returns_before_watching(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.000200"

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve") as appr_cls,
    ):
        RequestReview(_args(no_watch=True), mock_config, MagicMock(), prompt)()

    client.chat_postMessage.assert_called_once()
    prompt.metadata.set_slack_review.assert_called_once_with(
        mock_config.slack_channel, "1700000000.000200"
    )
    # Early return: never watched, never approved.
    client.wait_for_ack.assert_not_called()
    appr_cls.assert_not_called()


# ---------------------------------------------------------------------------
# --no-approve: watches + reports the ack but does not approve
# ---------------------------------------------------------------------------


def test_request_review_no_approve_watches_but_skips_approve(mock_config):
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.000300"
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
    client.chat_postMessage.return_value = "1700000000.000400"
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
    client.chat_postMessage.return_value = "1700000000.000500"
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


# ---------------------------------------------------------------------------
# BLOCKER regression: auto-approve after 👍 acts on the intended fanout
# template, not the prompt's active one.
# ---------------------------------------------------------------------------


def test_autoapprove_targets_fanout_template_not_active(mock_config):
    """Ack on the non-active report must approve *that* report's metadata.

    With several templates loaded, ``Command.run`` rebinds ``self.metadata``
    per template. The blocker was a bare ``Approve()`` reading the prompt's
    *active* metadata instead of the fanout iteration's target — a 👍 on
    report B could approve report A. This proves the constructed ``Approve``
    is bound to the fanout report's metadata/targets, not the active one.
    """
    prompt = _prompt()

    # The active template (what a buggy implementation would wrongly approve).
    active_meta = prompt.metadata

    # Two distinct loaded reports; ack lands on the NON-active one.
    report_active = MagicMock(name="report_active")
    report_active.id = "SUSE:Maintenance:1:1"
    report_active.targets = MagicMock(name="targets_active")
    _wire_marker(report_active)

    report_other = MagicMock(name="report_other")
    report_other.id = "SUSE:Maintenance:2:2"
    report_other.targets = MagicMock(name="targets_other")
    report_other.__bool__ = lambda self: True
    report_other._testreport_url.return_value = "http://qam/2/log"
    report_other.rrid = "SUSE:Maintenance:2:2"
    _wire_marker(report_other)

    prompt.templates.all.return_value = [report_active, report_other]

    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.000600"
    # Only the non-active report gets an ack; the active one times out.
    outcomes = {
        report_active: _outcome(acked=False, reviewer=None, timed_out=True),
        report_other: _outcome(acked=True, reviewer="dave"),
    }

    built: list[MagicMock] = []

    def _approve_factory(args, config, sys, prompt_):
        appr = MagicMock(name="Approve")
        built.append(appr)
        return appr

    def _wait_for_ack(channel, ts, **kw):
        # ``run`` sets ``cmd.metadata`` per template before ``__call__``; use it
        # to pick which report is being watched in this iteration.
        return outcomes[cmd.metadata]

    client.wait_for_ack.side_effect = _wait_for_ack

    cmd = RequestReview(_args(), mock_config, MagicMock(), prompt)

    with (
        patch("mtui.commands.request_review.svn_commit_testreport"),
        patch("mtui.commands.request_review.svn_update_testreport"),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve", side_effect=_approve_factory),
    ):
        cmd.run()

    # Exactly one approve was constructed + fired (the acked, non-active report).
    fired = [a for a in built if a.called]
    assert len(fired) == 1
    approved = fired[0]
    assert approved.metadata is report_other
    assert approved.targets is report_other.targets
    # And it was NOT the prompt's active metadata (the blocker symptom).
    assert approved.metadata is not active_meta


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


def test_request_review_marker_commit_failure_is_not_fatal(mock_config):
    """A failed marker commit is logged but does not abort the flow."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.000700"

    with (
        patch(
            "mtui.commands.request_review.svn_commit_testreport",
            side_effect=[None, subprocess.CalledProcessError(1, "svn")],
        ),
        patch("mtui.commands.request_review.SlackClient", return_value=client),
        patch("mtui.commands.request_review.Approve"),
    ):
        RequestReview(_args(no_watch=True), mock_config, MagicMock(), prompt)()

    # The marker was persisted and the no-watch path still completed.
    prompt.metadata.set_slack_review.assert_called_once_with(
        mock_config.slack_channel, "1700000000.000700"
    )


def test_request_review_unreachable_slack_reported(mock_config):
    """An unreachable-Slack outcome is reported distinctly and skips approve."""
    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.000800"
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


# ---------------------------------------------------------------------------
# tab completion
# ---------------------------------------------------------------------------


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
    client.chat_postMessage.return_value = "1700000000.000900"
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


def test_request_review_forwards_mcp_cancel_event(mock_config):
    """The MCP session's per-call cancel event reaches wait_for_ack."""
    import threading

    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.001000"
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
    client.chat_postMessage.return_value = "222.2"
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
    # The marker now records the new request.
    assert state["marker"] == (mock_config.slack_channel, "222.2")
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
    import threading

    prompt = _prompt()
    client = MagicMock()
    client.chat_postMessage.return_value = "1700000000.001100"

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


def test_request_review_superseded_marker_never_approves(mock_config):
    """An ack on a thread the report no longer records is not acted on."""
    prompt = _prompt()
    state = _wire_marker(prompt.metadata)
    client = MagicMock()
    client.chat_postMessage.return_value = "333.3"

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
