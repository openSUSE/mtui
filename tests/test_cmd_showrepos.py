"""Tests for the `show_update_repos` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.showrepos import Showrepos
from mtui.support.messages import TestReportNotLoadedError


def _prompt(metadata_truthy: bool = True) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: metadata_truthy
    p.metadata.update_repos = {"a": "b"}
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_showrepos_happy_calls_display(mock_config):
    prompt = _prompt()
    Showrepos(Namespace(), mock_config, MagicMock(), prompt)()
    prompt.display.list_update_repos.assert_called_once_with({"a": "b"})


def test_showrepos_without_metadata_raises(mock_config):
    prompt = _prompt(metadata_truthy=False)
    with pytest.raises(TestReportNotLoadedError):
        Showrepos(Namespace(), mock_config, MagicMock(), prompt)()
