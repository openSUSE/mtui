"""Tests for the `quit` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.cli import _history, repl
from mtui.commands.quit import Quit
from mtui.hosts.target.hostgroup import HostsGroup


def _make_target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(*hgs) -> MagicMock:
    """Build a stand-in for :class:`CommandPrompt` with the surface ``Quit`` reads.

    Accepts one or more :class:`HostsGroup` instances; each is wrapped in a
    fake report so ``quit`` closes every loaded template's hosts.
    """
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    reports = []
    for hg in hgs:
        report = MagicMock()
        report.targets = hg
        reports.append(report)
    p.targets = hgs[0] if hgs else {}
    p.templates.all.return_value = reports
    # ``Quit`` calls ``self.prompt._history.flush()`` to ensure the on-disk
    # history file is up to date. Replace it with a mock we can assert on;
    # the real ``FileHistory`` is exercised by the end-to-end test below.
    p._history = MagicMock()
    return p


def test_quit_exit_zero_no_bootarg(mock_config):
    t = _make_target("h1")
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)
    t.close.assert_called_once_with()
    prompt._history.flush.assert_called_once_with()


def test_quit_with_reboot_calls_close_with_reboot(mock_config):
    t = _make_target("h1")
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg="reboot")

    Quit(args, mock_config, sys_mock, prompt)()

    t.close.assert_called_once_with("reboot")
    sys_mock.exit.assert_called_once_with(0)
    prompt._history.flush.assert_called_once_with()


def test_quit_history_flush_failure_does_not_abort_exit(mock_config):
    """A broken history backend must not block process exit.

    The legacy code used ``contextlib.suppress(Exception)`` around
    ``readline.write_history_file``; the replacement keeps the same
    safety net around ``self.prompt._history.flush()``.
    """
    t = _make_target("h1")
    prompt = _prompt(HostsGroup([t]))
    prompt._history.flush.side_effect = OSError("disk full")
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)


@pytest.fixture
def _history_cache_reset():
    """Reset the memoised ``FileHistory`` cache between tests.

    ``mtui.cli._history.get_history`` memoises by resolved path so the
    REPL never opens two writers on the same file. Tests that point the
    history backend at a ``tmp_path`` location must therefore evict
    those entries afterwards so a later test cannot inherit a stale
    handle to a now-deleted tmp file.
    """
    snapshot = dict(_history._cache)
    yield
    _history._cache.clear()
    _history._cache.update(snapshot)


def test_quit_closes_all_loaded_templates(mock_config):
    """``quit`` disconnects every loaded template's hosts, not just the active."""
    t1 = _make_target("h1")
    t2 = _make_target("h2")
    prompt = _prompt(HostsGroup([t1]), HostsGroup([t2]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    Quit(args, mock_config, sys_mock, prompt)()

    t1.close.assert_called_once_with()
    t2.close.assert_called_once_with()
    sys_mock.exit.assert_called_once_with(0)


@pytest.mark.usefixtures("_history_cache_reset")
def test_quit_persists_history_to_disk(tmp_path, monkeypatch, mock_config):
    """End-to-end: a line appended to history survives ``quit``.

    Wires a real ``CommandPrompt`` (so the production ``FileHistory``
    pipeline is exercised) at a tmp-path history file, appends one
    entry, runs ``Quit``, and asserts the entry is on disk. Locks in
    the success-criterion "``~/.mtui_history`` is written across
    sessions" without depending on the user's real home directory.
    """
    history_path = tmp_path / ".mtui_history"
    monkeypatch.setattr("mtui.cli.repl.default_history_path", lambda: history_path)

    config = MagicMock()
    config.auto = False
    config.kernel = False
    sys_mock = MagicMock()
    sys_mock.exit.side_effect = SystemExit(0)

    p = repl.CommandPrompt(config, MagicMock(), sys_mock, MagicMock())
    p._history.append_string("show hosts")

    args = Namespace(bootarg=None)
    with pytest.raises(SystemExit):
        Quit(args, mock_config, sys_mock, p)()

    assert history_path.exists()
    assert b"+show hosts" in history_path.read_bytes()
