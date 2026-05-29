"""Tests for the `set_repo` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from contextlib import contextmanager
from unittest.mock import MagicMock, patch

from mtui.commands.setrepo import SetRepo
from mtui.target.hostgroup import HostsGroup
from mtui.target.locks import TargetLockedError


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


def test_set_repo_add_invokes_repo_manager(mock_config):
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(operation="add", hosts=None)

    @contextmanager
    def noop_ctx(_):
        yield

    with patch("mtui.commands.setrepo.LockedTargets", noop_ctx):
        SetRepo(args, mock_config, MagicMock(), prompt)()

    t.repo_manager.set.assert_called_once_with("add", prompt.metadata)


def test_set_repo_logs_when_target_locked(mock_config, caplog):
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(operation="add", hosts=None)
    caplog.set_level(logging.ERROR, logger="mtui.command.setrepo")

    @contextmanager
    def raise_locked(_):
        raise TargetLockedError("h1")
        yield  # pragma: no cover

    with patch("mtui.commands.setrepo.LockedTargets", raise_locked):
        SetRepo(args, mock_config, MagicMock(), prompt)()

    assert any("Target locked" in r.message for r in caplog.records)
