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

# A 👍 reaction as ``reactions_get`` returns it (message.reactions list).
_THUMBSUP = [{"name": "thumbsup", "count": 1, "users": ["Ureviewer"]}]


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
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


def _slack(reactions=None):
    """Patch the gate's ``SlackClient`` so ``reactions_get`` returns ``reactions``.

    The gate imports ``SlackClient`` lazily from ``mtui.data_sources`` inside
    ``require_slack_review``, so that is the patch target.
    """
    reactions = _THUMBSUP if reactions is None else reactions
    client = MagicMock()
    client.reactions_get.return_value = reactions
    return patch("mtui.data_sources.SlackClient", return_value=client)


# ---------------------------------------------------------------------------
# Happy path: with a live 👍 present the approve proceeds to the backend.
# ---------------------------------------------------------------------------


def test_approve_osc_branch_calls_osc_approve(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=False)

    with _slack(), patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_called_once_with(mock_config, prompt.metadata.rrid)
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    args = Namespace(group=None, user="", reviewer=None, force=False)

    with pytest.raises(TestReportNotLoadedError):
        Approve(args, mock_config, MagicMock(), prompt)()


def test_approve_pi_unlocks(mock_config):
    mock_config.lock_pi_autolock = True
    prompt = _prompt()
    prompt.metadata.rrid = RequestReviewID("SUSE:PI:34556:1")
    prompt.metadata.lock_comment = "testing of SUSE:PI:34556:1"
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=False)

    with _slack(), patch("mtui.commands.approve.OSC"):
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
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=False)

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
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=False)

    with (
        _slack(reactions=[{"name": "eyes", "count": 1, "users": ["Ux"]}]),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


def test_approve_proceeds_with_live_thumbsup(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=False)

    with _slack() as slack_cls, patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    slack_cls.return_value.reactions_get.assert_called_once_with("C123", "111.222")
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


# ---------------------------------------------------------------------------
# --force bypasses the gate interactively (REPL) but is inert non-interactively
# (MCP), where the gate still runs and refuses.
# ---------------------------------------------------------------------------


def test_approve_force_bypasses_gate_interactively(mock_config):
    prompt = _prompt()  # interactive=True
    prompt.metadata.slack_review = None  # no review at all
    prompt.metadata.get_slack_review.return_value = None
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=True)

    with _slack() as slack_cls, patch("mtui.commands.approve.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    # --force in the REPL skips the gate entirely: Slack is never consulted.
    slack_cls.assert_not_called()
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_force_inert_when_not_interactive(mock_config):
    prompt = _prompt()
    prompt.interactive = False  # McpSession -> force must not bypass the gate
    prompt.metadata.slack_review = None
    prompt.metadata.get_slack_review.return_value = None
    args = Namespace(group=["qam-sle"], user="", reviewer=None, force=True)

    with (
        _slack(),
        patch("mtui.commands.approve.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()


# ---------------------------------------------------------------------------
# approve -r REVIEWER: record reviewer + svn commit + approve
# ---------------------------------------------------------------------------


def test_approve_reviewer_records_commits_then_approves(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer="alice", force=False)

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
    args = Namespace(group=["qam-sle"], user="", reviewer="alice", force=False)

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
    args = Namespace(group=["qam-sle"], user="", reviewer="alice", force=False)

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
    args = Namespace(group=["qam-sle"], user="", reviewer="   ", force=False)

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
        "force": False,
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

    slack_cls.return_value.reactions_get.assert_called_once_with("C123", "111.222")
    osc_cls.return_value.reject.assert_called_once_with(["qam-sle"], "admin", "nope")


def test_reject_force_bypasses_gate_interactively(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()  # interactive=True
    prompt.metadata.slack_review = None
    prompt.metadata.get_slack_review.return_value = None

    with _slack() as slack_cls, patch("mtui.commands.apicall.OSC") as osc_cls:
        Reject(_reject_args(force=True), mock_config, MagicMock(), prompt)()

    slack_cls.assert_not_called()
    osc_cls.return_value.reject.assert_called_once_with(["qam-sle"], "admin", "nope")


def test_reject_force_inert_when_not_interactive(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    prompt.interactive = False
    prompt.metadata.slack_review = None
    prompt.metadata.get_slack_review.return_value = None

    with (
        _slack(),
        patch("mtui.commands.apicall.OSC") as osc_cls,
        pytest.raises(FailedSlackCallError),
    ):
        Reject(_reject_args(force=True), mock_config, MagicMock(), prompt)()

    osc_cls.assert_not_called()
