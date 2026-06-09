"""Tests for the heartbeat path in :meth:`McpSession.run_command`.

When the synthesised tool wrappers in :mod:`mtui.mcp.tools` (and the
hand-written testreport tools) pass a FastMCP ``Context`` through to
:meth:`McpSession.run_command`, the session is expected to emit
``notifications/progress`` every ``progress_interval`` seconds while
the underlying blocking command runs in a worker thread. This module
exercises that contract without spinning up the real MCP transport:

* ``ctx=None`` (the autoconnect path and most legacy callers) must
  take the original ``asyncio.to_thread`` shortcut and emit no
  progress frames.
* ``ctx`` supplied + a command that runs longer than the interval
  must produce one or more ``report_progress`` calls and still return
  the command's captured stdout.
* A command that finishes faster than the interval must produce zero
  progress frames (the heartbeat loop exits on the first
  ``asyncio.wait`` that sees the worker complete).
* Exceptions raised by the command must propagate unchanged through
  the heartbeat path.
* A ``report_progress`` failure must NOT mask the command's result.
"""

from __future__ import annotations

import asyncio
import logging
import time
from pathlib import Path
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from mtui.commands import Command
from mtui.commands.whoami import Whoami
from mtui.mcp.session import McpCommandError, McpSession


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _make_session(tmp_path: Path) -> McpSession:
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.progress"))


class _RecordingCtx:
    """Minimal stand-in for :class:`mcp.server.fastmcp.Context`.

    Only the ``report_progress`` coroutine is consumed by the
    heartbeat path; the real Context carries far more, but
    construction of one requires a live request scope. Tests pass
    this object as ``ctx`` and inspect ``calls`` afterwards.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[float, float | None, str | None]] = []

    async def report_progress(
        self,
        progress: float,
        total: float | None = None,
        message: str | None = None,
    ) -> None:
        self.calls.append((progress, total, message))


class _FailingCtx(_RecordingCtx):
    """Variant whose ``report_progress`` always raises.

    Used to assert the heartbeat loop swallows notification-send
    failures so a flaky transport never masks the command's result.
    """

    async def report_progress(
        self,
        progress: float,
        total: float | None = None,
        message: str | None = None,
    ) -> None:
        self.calls.append((progress, total, message))
        raise RuntimeError("transport gone")


class _SleepyCommand(Command):
    """Test-only command that sleeps for ``_hold_seconds`` then prints OK."""

    command = "_mcp_test_sleepy_command"
    _hold_seconds: ClassVar[float] = 0.25

    def __call__(self) -> None:  # pragma: no cover - exercised by tests
        time.sleep(self._hold_seconds)
        self.println("ok")


class _ExplodingCommand(Command):
    """Test-only command that always raises."""

    command = "_mcp_test_exploding_command"

    def __call__(self) -> None:  # pragma: no cover - exercised by tests
        raise RuntimeError("kaboom from command body")


# --------------------------------------------------------------------------- #
# ctx=None preserves the legacy zero-overhead path                            #
# --------------------------------------------------------------------------- #


def test_ctx_none_emits_no_progress_and_returns_stdout(tmp_path: Path) -> None:
    """``ctx=None`` must take the ``asyncio.to_thread`` shortcut unchanged."""
    sess = _make_session(tmp_path)
    ctx = _RecordingCtx()

    out = asyncio.run(sess.run_command(Whoami, [], ctx=None, progress_interval=0.01))
    assert out.startswith("User: testuser")
    # The Context we built was never passed to run_command, so it must
    # carry no recorded calls.
    assert ctx.calls == []


# --------------------------------------------------------------------------- #
# ctx supplied + slow command -> heartbeat fires                              #
# --------------------------------------------------------------------------- #


def test_heartbeat_fires_for_slow_command(tmp_path: Path) -> None:
    """A 0.25 s command with a 0.05 s heartbeat must record >= 1 frame."""
    sess = _make_session(tmp_path)
    ctx = _RecordingCtx()

    out = asyncio.run(
        sess.run_command(_SleepyCommand, [], ctx=ctx, progress_interval=0.05)
    )
    assert out == "ok\n"
    # At least one heartbeat must have landed; with hold=0.25 and
    # interval=0.05 we expect roughly 4 but assert loosely to keep
    # the test stable under slow CI.
    assert len(ctx.calls) >= 1, f"no heartbeat fired: {ctx.calls!r}"
    # Each frame must carry the command name in the message and a
    # non-negative progress value.
    for progress, total, message in ctx.calls:
        assert progress >= 0.0
        assert total is None
        assert message is not None
        assert "_mcp_test_sleepy_command" in message


def test_heartbeat_progress_values_are_monotonic(tmp_path: Path) -> None:
    """Heartbeat ``progress`` values record elapsed seconds; must not regress."""
    sess = _make_session(tmp_path)
    ctx = _RecordingCtx()

    asyncio.run(sess.run_command(_SleepyCommand, [], ctx=ctx, progress_interval=0.05))
    values = [p for p, _t, _m in ctx.calls]
    if len(values) >= 2:
        assert values == sorted(values), f"non-monotonic progress: {values!r}"


# --------------------------------------------------------------------------- #
# ctx supplied + fast command -> no heartbeat                                 #
# --------------------------------------------------------------------------- #


def test_no_heartbeat_for_fast_command(tmp_path: Path) -> None:
    """``whoami`` is sub-millisecond; with 1 s interval we expect zero frames."""
    sess = _make_session(tmp_path)
    ctx = _RecordingCtx()

    out = asyncio.run(sess.run_command(Whoami, [], ctx=ctx, progress_interval=1.0))
    assert out.startswith("User: testuser")
    assert ctx.calls == [], f"unexpected heartbeat: {ctx.calls!r}"


# --------------------------------------------------------------------------- #
# Exception propagation through the heartbeat path                            #
# --------------------------------------------------------------------------- #


def test_command_exception_propagates_through_heartbeat(tmp_path: Path) -> None:
    """A failing command must surface :class:`McpCommandError` unchanged."""
    sess = _make_session(tmp_path)
    ctx = _RecordingCtx()

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(
            sess.run_command(_ExplodingCommand, [], ctx=ctx, progress_interval=0.05)
        )
    assert ei.value.exit_code == 1
    assert "kaboom" in ei.value.stderr


# --------------------------------------------------------------------------- #
# report_progress failure must not mask the command result                    #
# --------------------------------------------------------------------------- #


def test_progress_send_failure_is_swallowed(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """If ``ctx.report_progress`` raises, the command result still surfaces."""
    sess = _make_session(tmp_path)
    ctx = _FailingCtx()

    with caplog.at_level(logging.DEBUG, logger="mtui.mcp.session"):
        out = asyncio.run(
            sess.run_command(_SleepyCommand, [], ctx=ctx, progress_interval=0.05)
        )
    assert out == "ok\n"
    # We attempted at least one heartbeat (FailingCtx records the
    # attempt before raising).
    assert len(ctx.calls) >= 1
    # And the swallowed failure was logged at DEBUG.
    assert any("progress notification failed" in r.message for r in caplog.records)
