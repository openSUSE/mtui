"""Tests for the `lrun` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.localrun import LocalRun


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_lrun_happy_passes_joined_command(mock_config):
    prompt = _prompt()
    args = Namespace(command=["echo", "hi"])
    with patch("mtui.commands.localrun.check_call") as cc:
        LocalRun(args, mock_config, MagicMock(), prompt)()
    cc.assert_called_once_with("echo hi", shell=True)


def test_lrun_empty_command_logs_error(mock_config, caplog):
    prompt = _prompt()
    args = Namespace(command=[])
    caplog.set_level(logging.ERROR, logger="mtui.commands.lrun")
    with patch("mtui.commands.localrun.check_call") as cc:
        LocalRun(args, mock_config, MagicMock(), prompt)()
    cc.assert_not_called()
    assert any("Missing argument" in r.message for r in caplog.records)
