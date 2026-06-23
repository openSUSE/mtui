"""Tests for the `load_template` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.loadtemplate import LoadTemplate
from mtui.support.messages import TestReportNotLoadedError


def _prompt(metadata_truthy: bool = False) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: metadata_truthy
    p.display = MagicMock()
    p.targets = {}
    return p


def test_load_template_auto_kind_loads(mock_config):
    """No prior metadata, kind=auto -> prompt.load_update is called.

    Workflow mode (auto/kernel) is now seeded onto the TestReport by
    ``make_testreport`` during ``load_update``, not by this command.
    """
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update, chosts=False)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    prompt.load_update.assert_called_once_with(update, autoconnect=True)


def test_load_template_unknown_kind_raises(mock_config):
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "unknown"
    args = Namespace(update=update, chosts=False)

    with pytest.raises(TestReportNotLoadedError):
        LoadTemplate(args, mock_config, MagicMock(), prompt)()
