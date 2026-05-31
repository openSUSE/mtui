"""Tests for the `shell` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.shell import Shell
from mtui.support.messages import HostIsNotConnectedError
from mtui.target.hostgroup import HostsGroup


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(targets) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = targets
    return p


def test_shell_invokes_shell_on_each_target(mock_config):
    t1, t2 = _target("h1"), _target("h2")
    prompt = _prompt(HostsGroup([t1, t2]))
    args = Namespace(hosts=None)

    Shell(args, mock_config, MagicMock(), prompt)()

    t1.shell.assert_called_once_with()
    t2.shell.assert_called_once_with()


def test_shell_unknown_host_propagates(mock_config):
    bad_targets = MagicMock()
    bad_targets.select.side_effect = HostIsNotConnectedError("ghost")
    prompt = _prompt(bad_targets)
    args = Namespace(hosts=["ghost"])
    with pytest.raises(HostIsNotConnectedError):
        Shell(args, mock_config, MagicMock(), prompt)()
