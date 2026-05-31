"""Tests for the `edit` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.edit import Edit
from mtui.support.messages import TestReportNotLoadedError


def _prompt(metadata_truthy: bool = True) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: metadata_truthy
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_edit_explicit_filename_invokes_editor(mock_config):
    prompt = _prompt()
    args = Namespace(filename="foo.txt")
    with (
        patch("mtui.commands.edit.getenv", return_value="vim"),
        patch("mtui.commands.edit.check_call") as cc,
    ):
        Edit(args, mock_config, MagicMock(), prompt)()
    cc.assert_called_once_with(["vim", "foo.txt"])


def test_edit_without_filename_or_metadata_raises(mock_config):
    prompt = _prompt(metadata_truthy=False)
    args = Namespace(filename=None)
    with pytest.raises(TestReportNotLoadedError):
        Edit(args, mock_config, MagicMock(), prompt)()
