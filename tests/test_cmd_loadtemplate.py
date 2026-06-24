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
    """kind=auto -> prompt.load_update is called (template is added).

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


def test_load_template_does_not_close_existing_hosts(mock_config):
    """Loading a second template must not tear down the active template's hosts.

    With multi-template support ``load_template`` adds rather than overwrites,
    so it never closes the previously active connections.
    """
    existing = MagicMock()
    prompt = _prompt(metadata_truthy=True)
    prompt.targets = {"h1": existing}
    update = MagicMock()
    update.kind = "auto"
    # -c not passed -> chosts defaults True (keep/re-add hosts).
    args = Namespace(update=update, chosts=True)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    existing.close.assert_not_called()
    prompt.load_update.assert_called_once_with(update, autoconnect=True)
    # carry-over re-adds the previously connected host to the new template
    prompt.metadata.add_target.assert_called_once_with("h1")


def test_load_template_clean_hosts_skips_carry_over(mock_config):
    """With -c/--clean-hosts (chosts False) no host carry-over happens."""
    prompt = _prompt(metadata_truthy=True)
    prompt.targets = {"h1": MagicMock()}
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update, chosts=False)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.add_target.assert_not_called()
