"""Tests for the `run` command."""

from __future__ import annotations

from argparse import Namespace
from contextlib import contextmanager
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.run import Run
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.hosts.target.locks import TargetLockedError
from mtui.support.messages import NoRefhostsDefinedError


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


def test_run_quotes_shell_metacharacters(mock_config):
    """A single argument with shell metacharacters must reach the host intact.

    Pre-fix the args were space-joined without quoting, so
    ``sh -c "VAR=x; echo $VAR"`` went on the wire as ``sh -c VAR=x; echo $VAR``
    -- the remote shell then ran ``sh -c VAR=x`` and leaked ``; echo $VAR`` into
    the outer shell, where ``$VAR`` expanded empty. ``shlex.join`` keeps the
    script body as one quoted argument.
    """
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(command=["sh", "-c", "VAR=x; echo $VAR"], hosts=None)

    @contextmanager
    def noop_ctx(_):
        yield

    with (
        patch("mtui.commands.run.LockedTargets", noop_ctx),
        patch("mtui.commands.run.page"),
        patch.object(HostsGroup, "run") as hg_run,
    ):
        Run(args, mock_config, MagicMock(), prompt)()

    hg_run.assert_called_once_with("sh -c 'VAR=x; echo $VAR'")


def test_run_target_locked_returns_without_unbound_local(mock_config):
    """A ``TargetLockedError`` during locking must not crash with ``UnboundLocalError``.

    Pre-fix, ``output`` was assigned inside the ``with LockedTargets(...)``
    block; when the context manager raised before assignment, the later
    ``page(output, ...)`` call referenced an undefined local. The fix
    lifts ``output: list[str] = []`` above the ``try:``, so this path
    must now return cleanly.
    """
    t = _target("h1")
    prompt = _prompt(HostsGroup([t]))
    args = Namespace(command=["uname"], hosts=None)

    @contextmanager
    def boom_ctx(_):
        raise TargetLockedError("locked by alice")
        yield  # unreachable, makes this a generator

    with (
        patch("mtui.commands.run.LockedTargets", boom_ctx),
        patch("mtui.commands.run.page") as page,
    ):
        # Must return cleanly (no UnboundLocalError, no re-raise).
        result = Run(args, mock_config, MagicMock(), prompt)()

    assert result is None
    # Early return short-circuits before page() is reached.
    page.assert_not_called()
