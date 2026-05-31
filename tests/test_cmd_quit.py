"""Tests for the `quit` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.quit import Quit
from mtui.hosts.target.hostgroup import HostsGroup


def _make_target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.homedir = "/tmp/home"
    p.targets = hg
    return p


def test_quit_exit_zero_no_bootarg(mock_config):
    t = _make_target("h1")
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    with patch("mtui.commands.quit.readline.write_history_file"):
        Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)
    t.close.assert_called_once_with()


def test_quit_with_reboot_calls_close_with_reboot(mock_config):
    t = _make_target("h1")
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg="reboot")

    with patch("mtui.commands.quit.readline.write_history_file"):
        Quit(args, mock_config, sys_mock, prompt)()

    t.close.assert_called_once_with("reboot")
    sys_mock.exit.assert_called_once_with(0)
