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


def test_load_template_auto_kind_sets_config_and_loads(mock_config):
    """No prior metadata, kind=auto -> config.auto=True and prompt.load_update."""
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update, chosts=False)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    assert mock_config.auto is True
    assert mock_config.kernel is False
    prompt.load_update.assert_called_once_with(update, autoconnect=True)


def test_load_template_unknown_kind_raises(mock_config):
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "unknown"
    args = Namespace(update=update, chosts=False)

    with pytest.raises(TestReportNotLoadedError):
        LoadTemplate(args, mock_config, MagicMock(), prompt)()
