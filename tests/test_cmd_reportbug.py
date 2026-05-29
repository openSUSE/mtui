"""Tests for the `report-bug` command."""

from __future__ import annotations

import errno
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui import messages
from mtui.commands.reportbug import ReportBug


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_report_bug_print_url_writes_url(mock_config):
    prompt = _prompt()
    sys_mock = MagicMock()
    mock_config.report_bug_url = "http://bugs.example.com"
    args = Namespace(print_url=True)

    ReportBug(args, mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "http://bugs.example.com" in written


def test_report_bug_missing_xdg_open_raises(mock_config):
    prompt = _prompt()
    mock_config.report_bug_url = "http://bugs.example.com"
    args = Namespace(print_url=False)

    with (
        patch(
            "mtui.commands.reportbug.subprocess.Popen",
            side_effect=OSError(errno.ENOENT, "no xdg"),
        ),
        pytest.raises(messages.SystemCommandNotFoundError),
    ):
        ReportBug(args, mock_config, MagicMock(), prompt)()
