"""Tests for the `reload_products` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.reload import ReloadProducts
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import HostIsNotConnectedError


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = hg
    return p


def test_reload_products_calls_reload_system_per_target(mock_config):
    t1, t2 = _target("h1"), _target("h2")
    prompt = _prompt(HostsGroup([t1, t2]))
    args = Namespace(hosts=None)

    ReloadProducts(args, mock_config, MagicMock(), prompt)()

    t1.reload_system.assert_called_once_with()
    t2.reload_system.assert_called_once_with()


def test_reload_products_unknown_host_propagates(mock_config):
    bad_targets = MagicMock()
    bad_targets.select.side_effect = HostIsNotConnectedError("ghost")
    prompt = _prompt(bad_targets)
    args = Namespace(hosts=["ghost"])
    with pytest.raises(HostIsNotConnectedError):
        ReloadProducts(args, mock_config, MagicMock(), prompt)()
