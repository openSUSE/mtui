"""Tests for the `terms` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.terms import Terms
from mtui.hosts.target.hostgroup import HostsGroup


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


def test_terms_no_termname_lists_available(mock_config):
    sys_mock = MagicMock()
    prompt = _prompt(HostsGroup([_target("h1")]))
    mock_config.termnames = ["xterm", "gnome"]
    args = Namespace(termname=None, hosts=None)

    Terms(args, mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "available terminals scripts:" in written
    assert "xterm gnome" in written


def test_terms_missing_termname_logs_error(mock_config, caplog):
    prompt = _prompt(HostsGroup([_target("h1")]))
    mock_config.termnames = ["xterm"]
    args = Namespace(termname="missing", hosts=None)
    caplog.set_level(logging.ERROR, logger="mtui.command.terms")

    Terms(args, mock_config, MagicMock(), prompt)()

    assert any("Term script not found" in r.message for r in caplog.records)
