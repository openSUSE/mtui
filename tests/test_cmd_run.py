"""Tests for the `run` command."""

from __future__ import annotations

from argparse import Namespace
from contextlib import contextmanager
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.run import Run
from mtui.messages import NoRefhostsDefinedError
from mtui.target.hostgroup import HostsGroup


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    t.lastin.return_value = "uname -a"
    t.lastexit.return_value = 0
    t.lastout.return_value = "Linux\n"
    t.lasterr.return_value = ""
    return t


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = hg
    return p


def test_run_happy_invokes_targets_run(mock_config):
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(command=["uname", "-a"], hosts=None)

    @contextmanager
    def noop_ctx(_):
        yield

    with (
        patch("mtui.commands.run.LockedTargets", noop_ctx),
        patch("mtui.commands.run.page") as page,
        patch.object(HostsGroup, "run") as hg_run,
    ):
        Run(args, mock_config, MagicMock(), prompt)()

    hg_run.assert_called_once_with("uname -a")
    page.assert_called_once()


def test_run_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(command=["x"], hosts=None)
    with pytest.raises(NoRefhostsDefinedError):
        Run(args, mock_config, MagicMock(), prompt)()
