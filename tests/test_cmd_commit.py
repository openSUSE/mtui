"""Tests for the `commit` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.commit import Commit
from mtui.support.messages import TestReportNotLoadedError


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
        patch("mtui.test_reports.svn_io.subprocess.check_call") as cc,
        patch("mtui.test_reports.svn_io.subprocess.call") as call,
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
        "mtui.test_reports.svn_io.subprocess.check_call", side_effect=OSError("boom")
    ):
        Commit(args, mock_config, MagicMock(), prompt)()

    assert any("committing template.failed" in r.message for r in caplog.records)


def _svn_ci_message(check_call_mock) -> str:
    """Return the message passed to the `svn ci -m <message>` invocation."""
    ci_calls = [
        c for c in check_call_mock.call_args_list if c.args[0][:2] == ["svn", "ci"]
    ]
    assert len(ci_calls) == 1
    cmd = ci_calls[0].args[0]
    assert cmd[2] == "-m"
    return cmd[3]


def test_commit_default_message_reuses_export_footer(mock_config, tmp_path):
    """No -m: commit non-interactively with the export-footer system info."""
    prompt = _prompt(tmp_path)
    mock_config.install_logs = "install_logs"
    mock_config.distro = "openSUSE Leap"
    mock_config.distro_ver = "16.0"
    mock_config.distro_kernel = "6.12.0"
    mock_config.session_user = "mpluskal"
    args = Namespace(msg=None)

    with (
        patch("mtui.test_reports.svn_io.subprocess.check_call") as cc,
        patch("mtui.test_reports.svn_io.subprocess.call"),
    ):
        Commit(args, mock_config, MagicMock(), prompt)()

    message = _svn_ci_message(cc)
    assert message.startswith("committed from MTUI:")
    assert "on openSUSE Leap-16.0 (kernel: 6.12.0) by mpluskal" in message
    assert not message.endswith("\n")  # rstripped for a tidy commit message


def test_commit_explicit_message_passed_through(mock_config, tmp_path):
    """An explicit -m message reaches svn verbatim -- no added quotes.

    The argv list is executed without a shell, so the old manual '"'
    wrapping stored literal double quotes in the SVN log message
    ('"my message"' instead of 'my message').
    """
    prompt = _prompt(tmp_path)
    mock_config.install_logs = "install_logs"
    args = Namespace(msg=[["my", "message"]])

    with (
        patch("mtui.test_reports.svn_io.subprocess.check_call") as cc,
        patch("mtui.test_reports.svn_io.subprocess.call"),
    ):
        Commit(args, mock_config, MagicMock(), prompt)()

    assert _svn_ci_message(cc) == "my message"


def test_commit_without_metadata_raises(mock_config):
    prompt = MagicMock()
    prompt.metadata = MagicMock()
    prompt.metadata.__bool__ = lambda self: False
    prompt.display = MagicMock()
    prompt.targets = MagicMock()
    with pytest.raises(TestReportNotLoadedError):
        Commit(Namespace(msg=None), mock_config, MagicMock(), prompt)()
