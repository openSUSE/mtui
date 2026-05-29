"""Tests for the `commit` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.commit import Commit
from mtui.messages import TestReportNotLoadedError


def _prompt(tmp: Path) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.report_wd.return_value = tmp
    p.metadata.fancy_report_url.return_value = "http://reports/x"
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_commit_happy_runs_svn_add_up_ci(mock_config, tmp_path):
    prompt = _prompt(tmp_path)
    mock_config.install_logs = "install_logs"
    args = Namespace(msg=None)

    with (
        patch("mtui.commands.commit.subprocess.check_call") as cc,
        patch("mtui.commands.commit.subprocess.call") as call,
    ):
        Commit(args, mock_config, MagicMock(), prompt)()

    # at least three check_calls (svn add install_logs, svn up, svn ci)
    assert cc.call_count >= 3
    # subprocess.call (for "results") is only invoked if the dir exists; here it does not
    call.assert_not_called()


def test_commit_swallows_subprocess_error(mock_config, tmp_path, caplog):
    prompt = _prompt(tmp_path)
    caplog.set_level(logging.ERROR, logger="mtui.command.commit")
    args = Namespace(msg=None)

    with patch(
        "mtui.commands.commit.subprocess.check_call", side_effect=OSError("boom")
    ):
        Commit(args, mock_config, MagicMock(), prompt)()

    assert any("committing template.failed" in r.message for r in caplog.records)


def test_commit_without_metadata_raises(mock_config):
    prompt = MagicMock()
    prompt.metadata = MagicMock()
    prompt.metadata.__bool__ = lambda self: False
    prompt.display = MagicMock()
    prompt.targets = MagicMock()
    with pytest.raises(TestReportNotLoadedError):
        Commit(Namespace(msg=None), mock_config, MagicMock(), prompt)()
