"""Tests for the `quit` command."""

from __future__ import annotations

import logging
import threading
from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.cli import _history, repl
from mtui.commands import quit as quit_module
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


def test_quit_close_failure_logs_warning_with_hostname(mock_config, caplog):
    """A crashing ``close()`` must not abort ``quit``, but must be visible.

    Teardown is best-effort, so the exit still happens with status 0;
    the failure has to surface as a warning naming the affected host.
    """
    t = _make_target("h1")
    t.close.side_effect = KeyError("boom")
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    with caplog.at_level(logging.WARNING, logger="mtui.command.quit"):
        Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)
    warnings = [
        r.getMessage()
        for r in caplog.records
        if r.name == "mtui.command.quit" and r.levelno == logging.WARNING
    ]
    assert any("h1" in m and "failed to disconnect" in m for m in warnings)


def test_quit_close_failure_names_the_failing_host_among_several(mock_config, caplog):
    """The warning must name the host whose ``close()`` actually failed.

    A single-host test cannot tell ``futures[future]`` (the correct
    per-future lookup) apart from a broken mapping that reports an
    arbitrary submitted host instead -- with only one host in the dict,
    any lookup trivially returns that same host. Load two hosts where
    only the second's ``close()`` raises and assert the failure warning
    names exactly that host, never the one that closed cleanly.
    """
    t1 = _make_target("h1")
    t2 = _make_target("h2")
    t2.close.side_effect = KeyError("boom")
    prompt = _prompt(HostsGroup([t1]), HostsGroup([t2]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    with caplog.at_level(logging.WARNING, logger="mtui.command.quit"):
        Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)
    t1.close.assert_called_once_with()
    warnings = [
        r.getMessage()
        for r in caplog.records
        if r.name == "mtui.command.quit" and r.levelno == logging.WARNING
    ]
    failures = [m for m in warnings if "failed to disconnect" in m]
    assert len(failures) == 1
    assert "h2" in failures[0]
    assert "h1" not in failures[0]


def test_quit_clean_teardown_emits_no_warnings(mock_config, caplog):
    """A fully successful teardown must not emit any warning at all.

    Guards the fidelity of the teardown-visibility warnings: a
    regression that logs unconditionally (e.g. iterating ``futures``
    instead of ``not_done``) would train users to ignore every quit,
    but the clean-path tests never inspected ``caplog`` before this.
    """
    t1 = _make_target("h1")
    t2 = _make_target("h2")
    prompt = _prompt(HostsGroup([t1]), HostsGroup([t2]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    with caplog.at_level(logging.WARNING, logger="mtui.command.quit"):
        Quit(args, mock_config, sys_mock, prompt)()

    sys_mock.exit.assert_called_once_with(0)
    warnings = [
        r.getMessage()
        for r in caplog.records
        if r.name == "mtui.command.quit" and r.levelno == logging.WARNING
    ]
    assert warnings == []


def test_quit_close_timeout_logs_warning_with_hostname(
    mock_config, caplog, monkeypatch
):
    """A ``close()`` that hangs past the teardown timeout is reported.

    The 45 s production timeout is patched down so the test stays fast;
    a log handler releases the hung worker the moment the timeout
    warning is emitted, so no arbitrary sleep is needed.
    """
    # ``raising=False``: the attribute only exists with the fix applied,
    # and revert-verification must fail on the assertion, not on setattr.
    monkeypatch.setattr(quit_module, "CLOSE_TIMEOUT", 0.05, raising=False)

    hang = threading.Event()
    t = _make_target("h1")
    t.close.side_effect = lambda *args: hang.wait()
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    class _ReleaseOnWarning(logging.Handler):
        """Unblocks the hung close as soon as the timeout warning fires."""

        def emit(self, record: logging.LogRecord) -> None:
            hang.set()

    release = _ReleaseOnWarning(level=logging.WARNING)
    quit_logger = logging.getLogger("mtui.command.quit")
    quit_logger.addHandler(release)
    # Backstop so a regression (warning never emitted) cannot hang the
    # test run forever on the executor-shutdown join.
    backstop = threading.Timer(5.0, hang.set)
    backstop.start()
    try:
        with caplog.at_level(logging.WARNING, logger="mtui.command.quit"):
            Quit(args, mock_config, sys_mock, prompt)()
    finally:
        quit_logger.removeHandler(release)
        hang.set()
        backstop.cancel()

    sys_mock.exit.assert_called_once_with(0)
    warnings = [
        r.getMessage()
        for r in caplog.records
        if r.name == "mtui.command.quit" and r.levelno == logging.WARNING
    ]
    assert any("h1" in m and "still disconnecting" in m for m in warnings)
    # No final-verdict "failed to disconnect" warning: the straggler went
    # on to succeed once released, so it must not also be reported as a
    # failure.
    assert not any("h1" in m and "failed to disconnect" in m for m in warnings)


def test_quit_straggler_that_later_raises_logs_failure_warning(
    mock_config, caplog, monkeypatch
):
    """A close still running at the timeout that later raises is not lost.

    Before this fix, only the "still disconnecting" timeout warning
    fired for a straggler; exiting the executor's ``with`` block still
    joins that worker (``Executor.__exit__`` is ``shutdown(wait=True)``),
    so quit blocks until the close actually finishes regardless -- but
    the exception it eventually raised was silently dropped. The
    straggler's failure reason must surface once the join completes.
    """
    monkeypatch.setattr(quit_module, "CLOSE_TIMEOUT", 0.05, raising=False)

    hang = threading.Event()

    def _close(*args):
        hang.wait()
        raise RuntimeError("late boom")

    t = _make_target("h1")
    t.close.side_effect = _close
    prompt = _prompt(HostsGroup([t]))
    sys_mock = MagicMock()
    args = Namespace(bootarg=None)

    class _ReleaseOnWarning(logging.Handler):
        """Unblocks the hung close as soon as the timeout warning fires."""

        def emit(self, record: logging.LogRecord) -> None:
            hang.set()

    release = _ReleaseOnWarning(level=logging.WARNING)
    quit_logger = logging.getLogger("mtui.command.quit")
    quit_logger.addHandler(release)
    # Backstop so a regression (warning never emitted) cannot hang the
    # test run forever on the executor-shutdown join.
    backstop = threading.Timer(5.0, hang.set)
    backstop.start()
    try:
        with caplog.at_level(logging.WARNING, logger="mtui.command.quit"):
            Quit(args, mock_config, sys_mock, prompt)()
    finally:
        quit_logger.removeHandler(release)
        hang.set()
        backstop.cancel()

    sys_mock.exit.assert_called_once_with(0)
    warnings = [
        r.getMessage()
        for r in caplog.records
        if r.name == "mtui.command.quit" and r.levelno == logging.WARNING
    ]
    assert any("h1" in m and "still disconnecting" in m for m in warnings)
    assert any(
        "h1" in m and "failed to disconnect" in m and "late boom" in m for m in warnings
    )


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
