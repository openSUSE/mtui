"""Tests for the `update` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.update import Update
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import NoRefhostsDefinedError


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
    args = Namespace(newpackage=None, noprepare=None, noscript=None, hosts=None)

    Update(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_update.assert_called_once()


def test_update_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(newpackage=None, noprepare=None, noscript=None, hosts=None)
    with pytest.raises(NoRefhostsDefinedError):
        Update(args, mock_config, MagicMock(), prompt)()
