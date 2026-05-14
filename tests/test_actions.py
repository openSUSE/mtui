"""Behaviour tests for ``mtui.target.actions``.

These tests pin the *desired* exception-propagation contract: a worker
callable that raises must surface its exception to the caller of
``run()``, instead of being silently swallowed by the worker thread.

Today's hand-rolled ``ThreadedMethod`` drains the queue with
``method(*parameter)`` inside a bare ``try/finally`` that calls
``task_done()`` but never ``.result()`` — the exception dies with the
thread. The tests below are marked ``xfail(strict=True)`` so they
visibly fail (then pass) once the executor-based rewrite lands.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest


@pytest.mark.xfail(
    strict=True,
    reason=(
        "ThreadedMethod silently swallows worker exceptions today; the "
        "executor rewrite surfaces them via Future.result()."
    ),
)
def test_threaded_target_group_run_propagates_worker_exception():
    """A ``FileDelete`` worker that raises must propagate to ``run()``."""
    from mtui.target.actions import FileDelete

    target = MagicMock()
    target.hostname = "h1"
    target.sftp_remove.side_effect = RuntimeError("delete failed")

    action = FileDelete([target], Path("/tmp/x"))
    with pytest.raises(RuntimeError, match="delete failed"):
        action.run()


@pytest.mark.xfail(
    strict=True,
    reason=(
        "RunCommand uses the same hand-rolled queue, so a failing "
        "Target.run is silently swallowed today."
    ),
)
def test_run_command_run_propagates_worker_exception():
    """A parallel ``Target.run`` failure must propagate to ``RunCommand.run()``."""
    from mtui.target.actions import RunCommand
    from mtui.types import ExecutionMode

    target = MagicMock()
    target.hostname = "h1"
    target.mode = ExecutionMode.PARALLEL
    target.run.side_effect = RuntimeError("ssh broke")

    cmd = RunCommand({"h1": target}, "true")
    with pytest.raises(RuntimeError, match="ssh broke"):
        cmd.run()
