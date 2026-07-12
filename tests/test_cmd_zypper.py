"""Tests for the `install` and `uninstall` (zypper) commands."""

from __future__ import annotations

import logging
import sys
from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.zypper import Install, Uninstall
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


def test_uninstall_help_describes_package_to_uninstall():
    """``uninstall --help`` must not carry Install's copy-pasted help text.

    Asserted directly on the parser action's ``help`` attribute rather than
    the rendered help text: argparse's ``HelpFormatter`` wraps to the
    terminal width (honoring a ``COLUMNS`` env var ahead of the (80, 24)
    fallback), so a narrow width could otherwise wrap "package to uninstall"
    across lines and make a substring check fail on correct code.
    """
    parser = Uninstall.argparser(sys)
    package_action = next(a for a in parser._actions if a.dest == "package")

    assert package_action.help == "package to uninstall"
