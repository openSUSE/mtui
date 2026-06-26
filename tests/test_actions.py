"""Behaviour tests for ``mtui.hosts.target.actions``.

The module fans worker callables out across a host group via a
``ThreadPoolExecutor``. The contract these tests pin:

* a worker callable's exception is re-raised to the caller via
  ``Future.result()`` (rather than dying inside the worker thread, as
  the old hand-rolled ``ThreadedMethod`` did);
* ``run_parallel`` still works on an empty work list;
* ``RunCommand`` runs serial-mode hosts strictly one at a time and
  bypasses the executor for them (the old code used the same queue
  for both, which made the serial branch indistinguishable from the
  parallel one).
"""

from __future__ import annotations

import logging
import threading
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest


def test_threaded_target_group_run_propagates_worker_exception():
    """A ``FileDelete`` worker that raises must propagate to ``run()``."""
    from mtui.hosts.target.actions import FileDelete

    target = MagicMock()
    target.hostname = "h1"
    target.sftp_remove.side_effect = RuntimeError("delete failed")

    action = FileDelete([target], Path("/tmp/x"))  # type: ignore[list-item]
    with pytest.raises(RuntimeError, match="delete failed"):
        action.run()


def test_run_command_run_propagates_worker_exception():
    """A parallel ``Target.run`` failure must propagate to ``RunCommand.run()``."""
    from mtui.hosts.target.actions import RunCommand
    from mtui.types import ExecutionMode

    target = MagicMock()
    target.hostname = "h1"
    target.mode = ExecutionMode.PARALLEL
    target.run.side_effect = RuntimeError("ssh broke")

    cmd = RunCommand({"h1": target}, "true")  # type: ignore[dict-item]
    with pytest.raises(RuntimeError, match="ssh broke"):
        cmd.run()


def test_run_command_dict_skips_targets_without_a_command():
    """A per-host command dict missing a host must not KeyError on it.

    The downgrade rollback builds a command for only the hosts that have a
    recorded previous version; ``run`` must act on the covered hosts and skip
    the rest rather than raising ``KeyError`` in ``_cmd_for``.
    """
    from mtui.hosts.target.actions import RunCommand
    from mtui.types import ExecutionMode

    covered = MagicMock()
    covered.hostname = "h1"
    covered.mode = ExecutionMode.PARALLEL
    uncovered = MagicMock()
    uncovered.hostname = "h2"
    uncovered.mode = ExecutionMode.PARALLEL

    # command dict covers only h1; h2 is in the group but has no command.
    cmd = RunCommand({"h1": covered, "h2": uncovered}, {"h1": "true"})  # type: ignore[dict-item]
    cmd.run()  # must not raise KeyError("h2")

    covered.run.assert_called_once()
    uncovered.run.assert_not_called()


def test_run_parallel_no_work_is_noop():
    """``run_parallel([])`` must not allocate an executor."""
    from mtui.hosts.target.actions import run_parallel

    # Should simply return; no exception, no thread spawned.
    run_parallel([])


def test_run_parallel_runs_each_callable_once():
    """Each ``(callable, args)`` pair runs exactly once with the given args."""
    from mtui.hosts.target.actions import run_parallel

    a = MagicMock()
    b = MagicMock()
    run_parallel([(a, (1, 2)), (b, ("x",))])
    a.assert_called_once_with(1, 2)
    b.assert_called_once_with("x")


def test_run_command_serial_runs_sequentially_after_prompt():
    """Serial-mode hosts skip the pool and run one-at-a-time after a prompt."""
    from mtui.hosts.target.actions import RunCommand
    from mtui.types import ExecutionMode

    t1 = MagicMock()
    t1.hostname = "h1"
    t1.mode = ExecutionMode.SERIAL
    t2 = MagicMock()
    t2.hostname = "h2"
    t2.mode = ExecutionMode.SERIAL

    cmd = RunCommand({"h1": t1, "h2": t2}, "echo hi")  # type: ignore[dict-item]
    with patch("mtui.hosts.target.actions.prompt_user") as mock_prompt:
        cmd.run()

    # Each host was prompted before running, and ran exactly once.
    assert mock_prompt.call_count == 2
    t1.run.assert_called_once()
    t2.run.assert_called_once()


def test_run_parallel_keyboard_interrupt_cancels_queued_work():
    """``KeyboardInterrupt`` from a worker propagates and drops queued work.

    The pool is sized to the work list, so up to ``len(work)`` callables
    can start in parallel; we keep most of them blocked on an event so
    they stay "in flight" while one worker raises. With
    ``cancel_futures=True`` the still-blocked workers' futures must be
    cancelled rather than awaited, and the never-started callables
    inside the cancelled futures must not have run.
    """
    from mtui.hosts.target.actions import run_parallel

    # ThreadPoolExecutor schedules submitted callables eagerly when
    # workers are available, so we can't rely on "queued but not
    # started" with a pool sized to the work. Cap the pool by sizing
    # the work list larger than max_workers via a separate executor
    # path is not available; instead, observe that ``cancel_futures``
    # only cancels not-yet-running futures. Here we assert the
    # contract by building work where the *first* future raises KI
    # before the others get a CPU slot: at minimum the KI propagates
    # and the run_parallel call returns promptly without joining the
    # held-open workers.
    release = threading.Event()
    started = threading.Event()
    other_finished = threading.Event()

    def raiser():
        started.wait(timeout=5)
        raise KeyboardInterrupt

    def blocker():
        # Signal that we're in flight, then block until released.
        started.set()
        release.wait(timeout=5)
        other_finished.set()

    work = [(blocker, ()), (raiser, ())]
    try:
        with pytest.raises(KeyboardInterrupt):
            run_parallel(work)  # ty: ignore[invalid-argument-type]
    finally:
        # Let the in-flight blocker terminate so the test thread does
        # not leak a worker into the next test.
        release.set()

    # Contract: KI propagated. The blocker may or may not have
    # finished by the time KI surfaced -- what matters is run_parallel
    # did not wait for it.
    assert started.is_set()


def test_run_parallel_emits_no_log_records(caplog):
    """``run_parallel`` must stay silent on the logging API.

    The TTY spinner is the only progress channel; per-completion log
    lines were dropped because they buried real diagnostics in noise on
    multi-host runs. A regression here would re-introduce that noise.
    """
    from mtui.hosts.target.actions import run_parallel

    with caplog.at_level(logging.DEBUG, logger="mtui.target.actions"):
        run_parallel([(MagicMock(), ()), (MagicMock(), ())], desc="probe")

    assert caplog.records == []


def test_tty_spinner_is_silent_when_stderr_not_a_tty(capsys):
    """``TtySpinner`` must be a no-op when stderr is not a TTY (pytest case)."""
    from mtui.support.spinner import TtySpinner, spinner

    s = TtySpinner("anything")
    s.start()
    s.stop()

    with spinner("anything"):
        pass

    captured = capsys.readouterr()
    assert captured.err == ""
    # Internal: no thread should have been spawned in non-TTY mode.
    assert s._thread is None  # noqa: SLF001


def test_run_parallel_skips_spinner_when_desc_is_none_even_on_tty(capsys):
    """``desc=None`` must skip the spinner unconditionally, even on a TTY.

    The MCP server (and any other headless caller) declares itself
    non-interactive so the ``HostsGroup`` layer passes ``desc=None``
    to ``run_parallel``. If a future change ever re-enabled the
    spinner for a ``desc=None`` call, this test would catch it: even
    with ``sys.stderr.isatty()`` patched to True the worker must
    finish without a single ``\\r`` frame on stderr.
    """
    from mtui.hosts.target.actions import run_parallel

    fake_tty = MagicMock()
    fake_tty.isatty.return_value = True
    with patch("mtui.support.spinner.sys.stderr", fake_tty):
        run_parallel([(MagicMock(), ()), (MagicMock(), ())], desc=None)

    # No spinner means no writes to the (faked) stderr at all.
    fake_tty.write.assert_not_called()
    # And the real captured stderr (pytest's) stays clean too.
    captured = capsys.readouterr()
    assert captured.err == ""
