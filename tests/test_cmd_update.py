"""Tests for the `update` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.update import Update
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import HostIsNotConnectedError, NoRefhostsDefinedError


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.display = MagicMock()
    p.targets = hg
    return p


def test_update_happy_calls_perform_update(mock_config):
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(newpackage=None, noprepare=None, hosts=None)

    Update(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_update.assert_called_once()


def test_update_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(newpackage=None, noprepare=None, hosts=None)
    with pytest.raises(NoRefhostsDefinedError):
        Update(args, mock_config, MagicMock(), prompt)()


def test_update_without_target_uses_all_enabled_hosts(mock_config):
    """With no ``-t``, perform_update receives every enabled host."""
    prompt = _prompt(HostsGroup([_target("h1"), _target("h2")]))
    args = Namespace(newpackage=None, noprepare=None, hosts=None)

    Update(args, mock_config, MagicMock(), prompt)()

    targets = prompt.metadata.perform_update.call_args.args[0]
    assert sorted(targets.names()) == ["h1", "h2"]


def test_update_target_narrows_to_named_subset(mock_config):
    """``update -t h1`` runs only on h1, leaving the rest of the group out."""
    prompt = _prompt(HostsGroup([_target("h1"), _target("h2")]))
    args = Namespace(newpackage=None, noprepare=None, hosts=["h1"])

    Update(args, mock_config, MagicMock(), prompt)()

    targets = prompt.metadata.perform_update.call_args.args[0]
    assert targets.names() == ["h1"]


def test_update_target_unknown_host_raises(mock_config):
    """A ``-t`` host that is not connected is rejected, not silently ignored."""
    prompt = _prompt(HostsGroup([_target("h1")]))
    args = Namespace(newpackage=None, noprepare=None, hosts=["nope"])

    with pytest.raises(HostIsNotConnectedError):
        Update(args, mock_config, MagicMock(), prompt)()
    prompt.metadata.perform_update.assert_not_called()
