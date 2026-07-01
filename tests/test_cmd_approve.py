"""Tests for the `approve` command (incl. the Slack review gate)."""

from __future__ import annotations

import subprocess
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.apicall import Reject
from mtui.commands.approve import Approve
from mtui.support.exceptions import FailedSlackCallError
from mtui.support.messages import TestReportNotLoadedError
from mtui.test_reports.svn_io import TemplateFormatError
from mtui.types import RequestKind, RequestReviewID

# A 👍 reaction as Slack message objects carry it (message.reactions list).
_THUMBSUP = [{"name": "thumbsup", "count": 1, "users": ["Ureviewer"]}]

# The update under test; the gate binds the Slack marker to this RRID by
# requiring the review-request parent message to name it verbatim.
_RRID = "SUSE:Maintenance:12345:67890"
_REVIEW_TEXT = f"Please review {_RRID}: https://qam.suse.de/testreports/x"


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
    p.metadata.rrid.__str__.return_value = _RRID
    # A persisted Slack review reference so the gate re-queries live reactions
    # rather than refusing outright; individual tests override as needed. The
    # gate reads the marker file-fresh via get_slack_review (any svn up may
    # have changed it), so that is what must be wired.
    p.metadata.slack_review = ("C123", "111.222")
    p.metadata.get_slack_review.return_value = ("C123", "111.222")
    p.interactive = True
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def _slack(reactions=None, *, text=None, messages=None, fetch_error=None):
    """Patch the gate's ``SlackClient`` for the marker-binding checks.

    The gate fetches the review-request parent message via
    ``conversations_replies`` and reads both its text (RRID binding) and its
    ``reactions`` (the 👍 ack) from it. By default the parent is a genuine
    review request for ``_RRID`` carrying a 👍; ``text``/``reactions``
    override the parent, ``messages`` replaces the whole thread, and
    ``fetch_error`` makes the fetch itself fail.

    The gate imports ``SlackClient`` lazily from ``mtui.data_sources`` inside
    ``require_slack_review``, so that is the patch target.
    """
    client = MagicMock()
    if fetch_error is not None:
        client.conversations_replies.side_effect = fetch_error
    else:
        if messages is None:
            messages = [
                {
                    "text": _REVIEW_TEXT if text is None else text,
                    "reactions": _THUMBSUP if reactions is None else reactions,
                }
            ]
        client.conversations_replies.return_value = messages
    return patch("mtui.data_sources.SlackClient", return_value=client)


# ---------------------------------------------------------------------------
# Happy path: with a live 👍 present the approve proceeds to the backend.
# ---------------------------------------------------------------------------


def test_approve_osc_branch_calls_osc_approve(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with _slack(), patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_called_once_with(mock_config, prompt.metadata.rrid)
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    args = Namespace(group=None, user="", reviewer=None)

    with pytest.raises(TestReportNotLoadedError):
        Approve(args, mock_config, MagicMock(), prompt)()


def test_approve_pi_unlocks(mock_config):
    mock_config.lock_pi_autolock = True
    prompt = _prompt()
    prompt.metadata.rrid = RequestReviewID("SUSE:PI:34556:1")
    prompt.metadata.lock_comment = "testing of SUSE:PI:34556:1"
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack(text="Please review SUSE:PI:34556:1: https://x"),
        patch("mtui.commands.approve.OSC"),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.targets.unlock.assert_called_once_with()
    assert prompt.metadata.lock_comment == ""


# ---------------------------------------------------------------------------
# Slack review gate: approve REFUSES without a recorded review or a live 👍.
# ---------------------------------------------------------------------------


def test_approve_refuses_when_no_slack_review(mock_config):
    prompt = _prompt()
    prompt.metadata.slack_review = None
    prompt.metadata.get_slack_review.return_value = None
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack() as slack_cls,
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    # No review recorded -> the gate never even talks to Slack, never approves.
    slack_cls.assert_not_called()
    osc_cls.assert_not_called()


def test_approve_refuses_when_no_thumbsup(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack(reactions=[{"name": "eyes", "count": 1, "users": ["Ux"]}]),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_approve_proceeds_with_live_thumbsup(mock_config):
    """Legitimate flow: parent names this RRID, matching channel, live 👍."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with _slack() as slack_cls, patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    slack_cls.return_value.conversations_replies.assert_called_once_with(
        "C123", "111.222"
    )
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_accepts_skin_toned_thumbsup(mock_config):
    """A skin-toned 👍 ("+1::skin-tone-3") on a genuine review message counts."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)
    reactions = [{"name": "+1::skin-tone-3", "count": 1, "users": ["Ureviewer"]}]

    with _slack(reactions=reactions), patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_passes_when_config_holds_channel_name(mock_config):
    """No false refusal when config holds a channel NAME but the marker holds
    the canonical channel ID: the binding is verified via the fetched message."""
    mock_config.slack_channel = "#qam-review"  # marker holds resolved id C123
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with _slack(), patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


# ---------------------------------------------------------------------------
# Marker binding: the plaintext 'Slack Review:' marker is forgeable (template
# edit, MCP testreport writes). The gate must refuse any 👍'd message that is
# not THIS update's review request.
# ---------------------------------------------------------------------------


def test_approve_refuses_forged_marker_without_rrid(mock_config):
    """A marker pointed at a random 👍'd message (text lacks the RRID) is refused."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack(text="team lunch at noon?"),  # carries a 👍, not a review request
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="does not mention"),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_approve_refuses_marker_for_other_update(mock_config):
    """A marker pointed at ANOTHER update's genuine review message is refused."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)
    other = "Please review SUSE:Maintenance:99999:11111: https://qam.suse.de/y"

    with (
        _slack(text=other),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="does not match this update"),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_approve_refuses_when_message_missing(mock_config):
    """A marker pointing at a nonexistent message (fetch fails) is refused."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack(fetch_error=FailedSlackCallError("thread_not_found")),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="could not be fetched"),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_approve_refuses_when_thread_is_empty(mock_config):
    """An empty conversations.replies result (no parent message) is refused."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with (
        _slack(messages=[]),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="does not exist"),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


# ---------------------------------------------------------------------------
# approve -r REVIEWER: record reviewer + svn commit + approve
# ---------------------------------------------------------------------------


def test_approve_reviewer_records_commits_then_approves(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer="alice")

    with (
        _slack(),
        patch("mtui.commands.approve.OSC") as osc_cls,
        patch("mtui.commands.approve.svn_commit_testreport") as svn,
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.set_reviewer.assert_called_once_with("alice")
    svn.assert_called_once()
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_reviewer_aborts_when_set_reviewer_fails(mock_config):
    prompt = _prompt()
    prompt.metadata.set_reviewer.side_effect = TemplateFormatError("no line")
    args = Namespace(group=["qam-sle"], user="", reviewer="alice")

    with (
        _slack(),
        patch("mtui.commands.approve.OSC") as osc_cls,
        patch("mtui.commands.approve.svn_commit_testreport") as svn,
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    svn.assert_not_called()
    osc_cls.assert_not_called()


def test_approve_reviewer_aborts_when_svn_commit_fails(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer="alice")

    with (
        _slack(),
        patch("mtui.commands.approve.OSC") as osc_cls,
        patch(
            "mtui.commands.approve.svn_commit_testreport",
            side_effect=subprocess.CalledProcessError(1, "svn"),
        ),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.set_reviewer.assert_called_once_with("alice")
    osc_cls.assert_not_called()


def test_approve_reviewer_empty_aborts(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer="   ")

    with (
        _slack(),
        patch("mtui.commands.approve.OSC") as osc_cls,
        patch("mtui.commands.approve.svn_commit_testreport") as svn,
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.set_reviewer.assert_not_called()
    svn.assert_not_called()
    osc_cls.assert_not_called()


# ---------------------------------------------------------------------------
# reject gate: mirrors approve. (Owned here because the pre-existing reject
# tests live in tests/test_cmd_apicall.py under another name — see follow_ups.)
# ---------------------------------------------------------------------------


def _reject_args(**over) -> Namespace:
    base = {
        "group": ["qam-sle"],
        "user": "",
        "reason": "admin",
        "message": ["nope"],
    }
    base.update(over)
    return Namespace(**base)


def test_reject_refuses_when_no_slack_review(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    prompt.metadata.slack_review = None
    prompt.metadata.get_slack_review.return_value = None

    with (
        _slack() as slack_cls,
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    slack_cls.assert_not_called()
    osc_cls.assert_not_called()


def test_reject_refuses_when_no_thumbsup(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()

    with (
        _slack(reactions=[{"name": "eyes", "count": 1, "users": ["Ux"]}]),
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_reject_proceeds_with_live_thumbsup(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()

    with _slack() as slack_cls, patch("mtui.commands.apicall.OSC") as osc_cls:
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    slack_cls.return_value.conversations_replies.assert_called_once_with(
        "C123", "111.222"
    )
    osc_cls.return_value.reject.assert_called_once_with(["qam-sle"], "admin", "nope")


def test_reject_refuses_forged_marker_without_rrid(mock_config):
    """The reject gate shares the marker binding: a 👍'd message whose text
    lacks this RRID is not a review request for this update."""
    mock_config.lock_pi_autolock = False
    prompt = _prompt()

    with (
        _slack(text="unrelated announcement everyone 👍'd"),
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="does not mention"),
    ):
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_reject_refuses_marker_for_other_update(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    other = "Please review SUSE:Maintenance:99999:11111: https://qam.suse.de/y"

    with (
        _slack(text=other),
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="does not match this update"),
    ):
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_reject_accepts_skin_toned_thumbsup(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    reactions = [{"name": "+1::skin-tone-2", "count": 1, "users": ["Ureviewer"]}]

    with _slack(reactions=reactions), patch("mtui.commands.apicall.OSC") as osc_cls:
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    osc_cls.return_value.reject.assert_called_once_with(["qam-sle"], "admin", "nope")


def test_reject_refuses_when_message_missing(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()

    with (
        _slack(fetch_error=FailedSlackCallError("thread_not_found")),
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError, match="could not be fetched"),
    ):
        Reject(_reject_args(), mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()
