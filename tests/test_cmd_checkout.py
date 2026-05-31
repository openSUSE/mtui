"""Tests for the `checkout` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.checkout import Checkout
from mtui.support.messages import TestReportNotLoadedError


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.report_wd.return_value = Path("/tmp/x")
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_checkout_happy_invokes_svn_up(mock_config):
    prompt = _prompt()
    with patch("mtui.commands.checkout.check_call") as cc:
        Checkout(Namespace(), mock_config, MagicMock(), prompt)()
    cc.assert_called_once_with(["svn", "up"], cwd=Path("/tmp/x"))


def test_checkout_swallows_subprocess_error(mock_config, caplog):
    prompt = _prompt()
    caplog.set_level(logging.ERROR, logger="mtui.command.checkout")
    with patch("mtui.commands.checkout.check_call", side_effect=OSError("boom")):
        Checkout(Namespace(), mock_config, MagicMock(), prompt)()
    assert any("updating template failed" in r.message for r in caplog.records)


def test_checkout_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        Checkout(Namespace(), mock_config, MagicMock(), prompt)()
