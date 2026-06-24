"""Tests for the `unlock` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.hostsunlock import HostsUnlock


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    targets = MagicMock()
    targets.select.return_value = MagicMock()
    p.targets = targets
    return p


def test_unlock_default_no_force(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, force=False, pool=False)
    HostsUnlock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.unlock.assert_called_once_with(force=False)


def test_unlock_with_force(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, force=True, pool=False)
    HostsUnlock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.unlock.assert_called_once_with(force=True)


def test_unlock_pool_default_no_force(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, force=False, pool=True)
    HostsUnlock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.pool_unlock.assert_called_once_with(force=False)
    prompt.targets.select.return_value.unlock.assert_not_called()


def test_unlock_pool_with_force(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, force=True, pool=True)
    HostsUnlock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.pool_unlock.assert_called_once_with(force=True)
