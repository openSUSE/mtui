"""Tests for the `install` (zypper) command."""

from __future__ import annotations

import logging
from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.zypper import Install
from mtui.hosts.target.hostgroup import HostsGroup


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


def test_install_happy_calls_perform_install(mock_config):
    t = _target("h1")
    hg = HostsGroup([t])
    prompt = _prompt(hg)
    args = Namespace(package=["bash"], hosts=None)

    Install(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_install.assert_called_once()
    _called_targets, called_pkgs = prompt.metadata.perform_install.call_args.args
    assert called_pkgs == ["bash"]


def test_install_swallows_exception(mock_config, caplog):
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    prompt.metadata.perform_install.side_effect = RuntimeError("boom")
    args = Namespace(package=["bash"], hosts=None)
    caplog.set_level(logging.CRITICAL, logger="mtui.command.zypper")

    Install(args, mock_config, MagicMock(), prompt)()

    assert any("failed to install" in r.message for r in caplog.records)
