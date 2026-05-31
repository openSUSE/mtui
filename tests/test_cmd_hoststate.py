"""Tests for the `set_host_state` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.hoststate import HostState
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.types import ExecutionMode


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


def test_set_host_state_serial_sets_mode(mock_config):
    t1, t2 = _target("h1"), _target("h2")
    prompt = _prompt(HostsGroup([t1, t2]))
    args = Namespace(state=["serial"], hosts=None)

    HostState(args, mock_config, MagicMock(), prompt)()

    assert t1.mode == ExecutionMode.SERIAL
    assert t2.mode == ExecutionMode.SERIAL


def test_set_host_state_enabled_sets_state_string(mock_config):
    t1, t2 = _target("h1"), _target("h2")
    prompt = _prompt(HostsGroup([t1, t2]))
    args = Namespace(state=["disabled"], hosts=None)

    HostState(args, mock_config, MagicMock(), prompt)()

    assert t1.state == "disabled"
    assert t2.state == "disabled"
