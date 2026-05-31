"""Tests for the `approve` command."""

from __future__ import annotations

import subprocess
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.approve import Approve
from mtui.support.messages import TestReportNotLoadedError
from mtui.test_reports.svn_io import TemplateFormatError
from mtui.types import RequestKind, RequestReviewID


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_approve_osc_branch_calls_osc_approve(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer=None)

    with patch("mtui.commands.approve.OSC") as osc_cls:
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

    with patch("mtui.commands.approve.OSC"):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.targets.unlock.assert_called_once_with()
    assert prompt.metadata.lock_comment == ""


# ---------------------------------------------------------------------------
# approve -r REVIEWER: record reviewer + svn commit + approve
# ---------------------------------------------------------------------------


def test_approve_reviewer_records_commits_then_approves(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", reviewer="alice")

    with (
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
        patch("mtui.commands.approve.OSC") as osc_cls,
        patch("mtui.commands.approve.svn_commit_testreport") as svn,
    ):
        Approve(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.set_reviewer.assert_not_called()
    svn.assert_not_called()
    osc_cls.assert_not_called()
