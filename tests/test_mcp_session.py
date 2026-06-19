"""Tests for :mod:`mtui.mcp.session`.

Covers the four behaviours listed in PLAN.md step 5:

* ``run_command`` runs a real registered command and returns its stdout.
* argparse failure surfaces as :class:`McpCommandError` with a non-zero
  ``exit_code``.
* The session-wide lock serialises concurrent ``run_command`` calls.
* ``set_prompt`` records the session label.

The tests use ``asyncio.run`` rather than ``pytest-asyncio`` (the dev
group does not pull it in); each test is a tiny synchronous wrapper.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import logging
import threading
import time
from pathlib import Path
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from mtui.commands import Command
from mtui.commands.whoami import Whoami
from mtui.mcp.session import McpCommandError, McpSession
from mtui.support.concurrency import ContextExecutor


def _config(tmp_path: Path) -> MagicMock:
    """Build a MagicMock Config with just the attributes NullTestReport reads."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _make_session(tmp_path: Path) -> McpSession:
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.session"))


# --------------------------------------------------------------------------- #
# Construction                                                                #
# --------------------------------------------------------------------------- #


def test_construction_exposes_command_prompt_surface(tmp_path: Path) -> None:
    """:class:`McpSession` must expose the attributes ``Command.__init__`` reads."""
    sess = _make_session(tmp_path)
    assert sess.interactive is False
    assert sess.prompter is None
    assert sess.session is None
    assert sess.metadata is not None  # NullTestReport
    assert bool(sess.metadata) is False
    assert sess.targets is sess.metadata.targets
    # Registry snapshot present and includes every concrete command.
    for name in Command.registry:
        assert name in sess.commands


# --------------------------------------------------------------------------- #
# run_command happy path                                                      #
# --------------------------------------------------------------------------- #


def test_run_command_whoami_returns_stdout(tmp_path: Path) -> None:
    """``whoami`` produces the same ``User: …`` line the REPL prints."""
    sess = _make_session(tmp_path)
    out = asyncio.run(sess.run_command(Whoami, []))
    assert out.startswith("User: testuser, app pid: ")
    assert out.endswith("\n")


# --------------------------------------------------------------------------- #
# argparse failure                                                            #
# --------------------------------------------------------------------------- #


def test_run_command_argparse_failure_raises(tmp_path: Path) -> None:
    """Unknown flags raise :class:`McpCommandError` with a non-zero status."""
    sess = _make_session(tmp_path)
    with pytest.raises(McpCommandError) as ei:
        asyncio.run(sess.run_command(Whoami, ["--bogus"]))
    assert ei.value.exit_code != 0
    # argparse writes its complaint to stderr; the error renders it.
    assert "bogus" in str(ei.value) or "bogus" in ei.value.stderr


# --------------------------------------------------------------------------- #
# Lock serialisation                                                          #
# --------------------------------------------------------------------------- #


class _RecordingCommand(Command):
    """Test-only command that records its own start/end timestamps.

    Sleeps briefly so concurrent invocations would overlap if the lock
    failed to serialise them.
    """

    command = "_mcp_test_recording_command"
    _intervals: ClassVar[list[tuple[float, float]]] = []
    _hold_seconds: ClassVar[float] = 0.05

    def __call__(self) -> None:  # pragma: no cover - exercised by test
        start = time.monotonic()
        time.sleep(self._hold_seconds)
        end = time.monotonic()
        type(self)._intervals.append((start, end))
        self.println(f"{start:.6f}-{end:.6f}")


def test_run_command_serialises_via_lock(tmp_path: Path) -> None:
    """Two concurrent ``run_command`` calls must not overlap in time."""
    sess = _make_session(tmp_path)
    _RecordingCommand._intervals.clear()

    async def driver() -> None:
        await asyncio.gather(
            sess.run_command(_RecordingCommand, []),
            sess.run_command(_RecordingCommand, []),
            sess.run_command(_RecordingCommand, []),
        )

    asyncio.run(driver())

    intervals = sorted(_RecordingCommand._intervals)
    assert len(intervals) == 3
    # Strict non-overlap: each interval ends at-or-before the next starts.
    for (_a_start, a_end), (b_start, _b_end) in zip(
        intervals, intervals[1:], strict=False
    ):
        assert a_end <= b_start, f"intervals overlapped: {intervals!r}"


# --------------------------------------------------------------------------- #
# set_prompt                                                                  #
# --------------------------------------------------------------------------- #


def test_set_prompt_records_session_label(tmp_path: Path) -> None:
    sess = _make_session(tmp_path)
    sess.set_prompt("SUSE:Maintenance:1:1")
    assert sess.session == "SUSE:Maintenance:1:1"
    sess.set_prompt(None)
    assert sess.session is None


# --------------------------------------------------------------------------- #
# Log capture into the reply                                                  #
# --------------------------------------------------------------------------- #


class _LoggingCommand(Command):
    """Test-only command that logs at several levels via the ``mtui`` tree."""

    command = "_mcp_test_logging_command"

    def __call__(self) -> None:  # pragma: no cover - exercised by test
        log = logging.getLogger("mtui.commands._mcp_test_logging_command")
        log.warning("drift on h1")
        log.info("connected h1")
        log.debug("low-level noise")
        self.println("stdout line")


class _QuietCommand(Command):
    """Test-only command that logs nothing and prints one line."""

    command = "_mcp_test_quiet_command"

    def __call__(self) -> None:  # pragma: no cover - exercised by test
        self.println("quiet line")


def test_run_command_captures_mtui_log_records_into_reply(tmp_path: Path) -> None:
    """INFO+ records a command logs via the ``mtui`` tree land in the reply."""
    sess = _make_session(tmp_path)
    out = asyncio.run(sess.run_command(_LoggingCommand, []))

    assert "WARNING: drift on h1" in out
    assert "INFO: connected h1" in out
    # The command's own stdout is preserved alongside the captured logs.
    assert "stdout line" in out
    # DEBUG stays below the INFO capture threshold.
    assert "low-level noise" not in out


def test_run_command_log_capture_does_not_leak_across_calls(tmp_path: Path) -> None:
    """The capture handler is detached in ``finally``; a later reply is clean."""
    sess = _make_session(tmp_path)

    first = asyncio.run(sess.run_command(_LoggingCommand, []))
    assert "WARNING: drift on h1" in first

    # A subsequent command that logs nothing must not inherit the first
    # call's handler nor any of its records.
    second = asyncio.run(sess.run_command(_QuietCommand, []))
    assert second == "quiet line\n"
    assert "drift on h1" not in second

    # And no handler was left attached to the shared 'mtui' logger.
    mtui_logger = logging.getLogger("mtui")
    assert not any(
        type(h).__name__ == "_LogCaptureHandler" for h in mtui_logger.handlers
    )


def test_run_command_captures_context_executor_worker_records(tmp_path: Path) -> None:
    """Records logged on a ``ContextExecutor`` worker thread are captured.

    This is the real-world path: ``add_host`` fans out host connects to a
    thread pool, and the product-drift warnings are logged on those pool
    workers. ``ContextExecutor`` propagates the per-call capture token
    into the worker, so those records reach the reply.
    """
    sess = _make_session(tmp_path)

    def _emit_drift(host: str) -> None:
        logging.getLogger("mtui.commands._mcp_test_pool_worker").warning(
            "drift on %s", host
        )

    class _PoolCommand(Command):
        command = "_mcp_test_pool_command"

        def __call__(self) -> None:  # pragma: no cover - exercised by test
            with ContextExecutor() as ex:
                list(
                    concurrent.futures.as_completed(
                        [ex.submit(_emit_drift, h) for h in ("h1", "h2")]
                    )
                )
            self.println("main line")

    out = asyncio.run(sess.run_command(_PoolCommand, []))
    assert "main line" in out
    # Worker-thread warnings are captured because ContextExecutor carried
    # the call's capture token into the pool threads.
    assert "WARNING: drift on h1" in out
    assert "WARNING: drift on h2" in out


def test_run_command_excludes_records_without_capture_token(tmp_path: Path) -> None:
    """A record on a raw thread (no token propagation) is not captured.

    A bare :class:`threading.Thread` does not inherit the call's
    contextvars, so its records carry no capture token and must stay out
    of the reply — this is what keeps concurrent sessions isolated.
    """
    sess = _make_session(tmp_path)

    def _emit_from_raw_thread() -> None:
        logging.getLogger("mtui.commands._mcp_test_raw_thread").warning(
            "from a raw thread"
        )

    class _RawThreadCommand(Command):
        command = "_mcp_test_raw_thread_command"

        def __call__(self) -> None:  # pragma: no cover - exercised by test
            t = threading.Thread(target=_emit_from_raw_thread)
            t.start()
            t.join()
            self.println("main line")

    out = asyncio.run(sess.run_command(_RawThreadCommand, []))
    assert "main line" in out
    assert "from a raw thread" not in out
