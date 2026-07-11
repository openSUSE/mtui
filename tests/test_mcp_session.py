"""Tests for :mod:`mtui.mcp.session`.

Covers the four behaviours listed in PLAN.md step 5:

* ``run_command`` runs a real registered command and returns its stdout.
* argparse failure surfaces as :class:`McpCommandError` with a non-zero
  ``exit_code``.
* The session-wide lock serialises concurrent ``run_command`` calls.
* ``set_prompt`` is a no-op stub for ``CommandPrompt`` parity.

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


@pytest.fixture(autouse=True)
def _unbind_test_commands():
    """Drop command registrations a test makes in its own body.

    ``Command.__init_subclass__`` binds names process-globally at class
    creation; a test-local class would collide with itself when pytest
    re-runs in the same interpreter (mutmut re-enters pytest.main), so
    remove whatever a test added once it finishes.
    """
    before = set(Command.registry)
    yield
    for name in set(Command.registry) - before:
        del Command.registry[name]


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


def test_run_command_unscoped_serialises_via_exclusive_gate(tmp_path: Path) -> None:
    """Unscoped commands (no real template) take the exclusive registry gate.

    With nothing loaded, ``_resolve_job_rrids`` yields only the Null report, so
    the command falls onto the registry-exclusive path and concurrent calls
    must still run strictly one-at-a-time (no overlap).
    """
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


def _rrid_recorder_command():
    """Build a throwaway fan-out :class:`Command` that records its interval.

    Each ``__call__`` sleeps briefly and appends ``(rrid, start, end)`` to the
    returned ``seen`` list, so a test can check whether two invocations on
    different RRIDs overlapped in time. ``scope="fanout"`` + ``_add_template_arg``
    lets ``-T <rrid>`` scope it to exactly one loaded template (one per-RRID
    lock). The caller must unregister ``cls`` afterwards.
    """
    seen: list[tuple[str, float, float]] = []

    class _RridProbe(Command):
        command = "rrid_probe_tmp"
        scope: ClassVar[str] = "fanout"
        _hold_seconds: ClassVar[float] = 0.1

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            start = time.monotonic()
            time.sleep(self._hold_seconds)
            end = time.monotonic()
            seen.append((str(self.metadata.id), start, end))

    return _RridProbe, seen


def test_run_command_different_rrids_run_concurrently(tmp_path: Path) -> None:
    """Two ``run_command`` calls scoped to *different* templates overlap in time."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls, seen = _rrid_recorder_command()

    async def driver() -> None:
        await asyncio.gather(
            sess.run_command(cls, ["-T", "SUSE:Maintenance:1:1"]),
            sess.run_command(cls, ["-T", "SUSE:Maintenance:2:1"]),
        )

    try:
        asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    assert len(seen) == 2
    (_r1, s1, e1), (_r2, s2, e2) = seen
    # Different RRID locks → the intervals must overlap (each holds for 0.1s,
    # so a serial run would take ~0.2s and not overlap).
    assert s2 < e1, f"expected overlap, got {seen!r}"
    assert s1 < e2, f"expected overlap, got {seen!r}"


def test_run_command_same_rrid_serialises(tmp_path: Path) -> None:
    """Two ``run_command`` calls scoped to the *same* template do not overlap."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls, seen = _rrid_recorder_command()

    async def driver() -> None:
        await asyncio.gather(
            sess.run_command(cls, ["-T", "SUSE:Maintenance:1:1"]),
            sess.run_command(cls, ["-T", "SUSE:Maintenance:1:1"]),
        )

    try:
        asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    assert len(seen) == 2
    intervals = sorted((s, e) for _r, s, e in seen)
    (_s1, e1), (s2, _e2) = intervals
    assert e1 <= s2, f"same-RRID calls overlapped: {seen!r}"


def _stdout_probe_command():
    """Throwaway command that prints its RRID twice around a sleep.

    Two concurrent different-RRID runs overlap (per-RRID locks); each call must
    capture *only* its own two prints — proving ``self._current_stdout`` /
    ``self.display`` are per-call context vars, not a clobberable session attr.
    """

    class _StdoutProbe(Command):
        command = "stdout_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            rrid = str(self.metadata.id)
            self.println(f"{rrid}:first")
            time.sleep(0.1)
            self.println(f"{rrid}:second")

    return _StdoutProbe


def test_concurrent_runs_do_not_clobber_each_others_stdout(tmp_path: Path) -> None:
    """Overlapping different-RRID runs each capture only their own output."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls = _stdout_probe_command()

    async def driver() -> tuple[str, str]:
        return await asyncio.gather(
            sess.run_command(cls, ["-T", "SUSE:Maintenance:1:1"]),
            sess.run_command(cls, ["-T", "SUSE:Maintenance:2:1"]),
        )

    try:
        out_a, out_b = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    # Each reply contains exactly its own RRID's two lines and nothing of the
    # other call's — no cross-contamination despite the overlapping execution.
    assert out_a == "SUSE:Maintenance:1:1:first\nSUSE:Maintenance:1:1:second\n"
    assert out_b == "SUSE:Maintenance:2:1:first\nSUSE:Maintenance:2:1:second\n"


# --------------------------------------------------------------------------- #
# set_prompt                                                                  #
# --------------------------------------------------------------------------- #


def test_set_prompt_is_a_noop(tmp_path: Path) -> None:
    """``set_prompt`` exists for ``CommandPrompt`` parity and takes no args."""
    sess = _make_session(tmp_path)
    # Must be callable with no arguments and must not raise.
    assert sess.set_prompt() is None


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


def test_concurrent_captures_keep_info_after_first_call_finishes(
    tmp_path: Path,
) -> None:
    """A finishing command must not restore the logger level under a peer.

    The ``mtui`` logger is process-global; two commands (different client
    sessions here, different templates in production) capture
    concurrently. The plain save/lower/restore raced: the first call
    lowered WARNING->INFO, the overlapping call saw INFO and saved
    nothing, then the first call's ``finally`` restored WARNING while the
    second was still running — its INFO records were filtered at the
    logger before its capture handler saw them and silently vanished from
    the MCP reply. The lowering is now reference-counted; only the last
    concurrent capture restores.
    """
    mtui_logger = logging.getLogger("mtui")
    prev_level = mtui_logger.level
    mtui_logger.setLevel(logging.WARNING)
    a_entered = threading.Event()
    a_proceed = threading.Event()
    b_entered = threading.Event()
    b_proceed = threading.Event()

    class _FirstCommand(Command):
        command = "_mcp_test_levelrace_first"

        def __call__(self) -> None:  # pragma: no cover - exercised by test
            a_entered.set()
            assert a_proceed.wait(15)
            self.println("first out")

    class _SecondCommand(Command):
        command = "_mcp_test_levelrace_second"

        def __call__(self) -> None:  # pragma: no cover - exercised by test
            b_entered.set()
            assert b_proceed.wait(15)
            # Logged after the first command has finished (and, before the
            # fix, restored the level to WARNING out from under us).
            logging.getLogger("mtui.commands._levelrace").info("late info")
            self.println("second out")

    try:
        sess_a = _make_session(tmp_path)
        sess_b = _make_session(tmp_path)

        async def driver() -> tuple[str, str]:
            a_task = asyncio.create_task(sess_a.run_command(_FirstCommand, []))
            await asyncio.to_thread(a_entered.wait, 15)  # A lowered the level
            b_task = asyncio.create_task(sess_b.run_command(_SecondCommand, []))
            await asyncio.to_thread(b_entered.wait, 15)  # B's capture is live
            a_proceed.set()
            out_a = await a_task  # A finishes mid-B
            b_proceed.set()
            out_b = await b_task
            return out_a, out_b

        out_a, out_b = asyncio.run(driver())

        assert "first out" in out_a
        assert "second out" in out_b
        assert "INFO: late info" in out_b
        # The last concurrent capture restored the original level.
        assert mtui_logger.level == logging.WARNING
    finally:
        a_proceed.set()
        b_proceed.set()
        mtui_logger.setLevel(prev_level)


# --------------------------------------------------------------------------- #
# close() releases host-arbitration pool claims (Phase 3B)                    #
# --------------------------------------------------------------------------- #


def test_close_releases_pool_claims(tmp_path: Path) -> None:
    """``close`` calls ``release_pool_claims`` on every loaded template."""
    sess = _make_session(tmp_path)
    report = MagicMock()
    report.id = "SUSE:Maintenance:1:1"
    report.targets = {}
    sess.templates.add(report)
    sess.templates.set_active("SUSE:Maintenance:1:1")

    asyncio.run(sess.close())

    report.release_pool_claims.assert_called_once()


def test_close_disconnects_every_loaded_templates_hosts(tmp_path: Path) -> None:
    """``close`` closes hosts on *all* loaded templates, not just the active one."""
    sess = _make_session(tmp_path)

    active_host = MagicMock()
    other_host = MagicMock()

    active = MagicMock()
    active.id = "SUSE:Maintenance:1:1"
    active.targets = {"active-host": active_host}
    other = MagicMock()
    other.id = "SUSE:Maintenance:2:1"
    other.targets = {"other-host": other_host}

    sess.templates.add(active)
    sess.templates.add(other)
    sess.templates.set_active("SUSE:Maintenance:1:1")

    asyncio.run(sess.close())

    # Both the active and the non-active template's hosts are disconnected.
    active_host.close.assert_called_once()
    other_host.close.assert_called_once()
    # And each template's host group is emptied.
    assert active.targets == {}
    assert other.targets == {}


def test_disconnect_targets_bounded_wait_survives_a_wedged_close(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """A host whose ``close()`` never returns must not block teardown.

    ``Executor.__exit__`` runs ``shutdown(wait=True)``, so bounding the
    wait with only the ``with`` block would re-block on a wedged paramiko
    close and hang ``close()`` (and the http idle-sweep behind it)
    forever. The explicit ``shutdown(wait=False)`` keeps the timed wait
    real: teardown returns despite the stuck close, the healthy host is
    still closed, the stuck one is reported, and every template's host
    group is cleared.
    """
    sess = _make_session(tmp_path)

    release = threading.Event()
    wedged_host = MagicMock()
    wedged_host.close.side_effect = lambda: release.wait(30)
    good_host = MagicMock()

    report = MagicMock()
    report.id = "SUSE:Maintenance:1:1"
    report.targets = {"wedged-host": wedged_host, "good-host": good_host}
    sess.templates.add(report)
    sess.templates.set_active("SUSE:Maintenance:1:1")

    worker = threading.Thread(target=sess._disconnect_targets, kwargs={"timeout": 0.2})
    try:
        with caplog.at_level(logging.WARNING):
            worker.start()
            # Generous guard: the fix returns in ~0.2 s; a regression that
            # relies on the ``with`` exit would still be blocked here.
            worker.join(timeout=15)

        assert not worker.is_alive()
        # The healthy host was closed even though a sibling close hung.
        good_host.close.assert_called_once()
        # The stuck host was reported, not silently dropped.
        assert any(
            "wedged-host" in r.getMessage() and "did not disconnect" in r.getMessage()
            for r in caplog.records
        )
        # Host groups are emptied regardless of the stuck close.
        assert report.targets == {}
    finally:
        # Let the abandoned worker thread finish so it does not linger.
        release.set()
        worker.join(timeout=5)


# --------------------------------------------------------------------------- #
# Fan-out dispatch honours the ``template`` parameter (Phase 4)               #
# --------------------------------------------------------------------------- #


def _load_two_reports(sess: McpSession) -> tuple[MagicMock, MagicMock]:
    """Add two MagicMock reports to ``sess`` and return them (a active)."""
    a = MagicMock()
    a.id = "SUSE:Maintenance:1:1"
    a.targets = {}
    b = MagicMock()
    b.id = "SUSE:Maintenance:2:1"
    b.targets = {}
    sess.templates.add(a)
    sess.templates.add(b)
    sess.templates.set_active("SUSE:Maintenance:1:1")
    return a, b


def _fanout_recorder_command():
    """Build a throwaway fan-out :class:`Command` recording invoked RRIDs.

    Returns ``(cls, seen)`` where ``seen`` is the list each ``__call__``
    appends its acting template's RRID to. The caller must unregister
    ``cls`` from ``Command.registry`` afterwards.
    """
    seen: list[str] = []

    class _FanoutProbe(Command):
        command = "fanout_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            seen.append(str(self.metadata.id))

    return _FanoutProbe, seen


def test_dispatch_fanout_runs_every_template(tmp_path: Path) -> None:
    """A fan-out command with no ``-T`` runs once per loaded template."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls, seen = _fanout_recorder_command()
    try:
        asyncio.run(sess.run_command(cls, []))
    finally:
        Command.registry.pop(cls.command, None)
    assert seen == ["SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"]


def test_dispatch_template_flag_scopes_to_one(tmp_path: Path) -> None:
    """``-T <rrid>`` scopes a fan-out command to that single template."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls, seen = _fanout_recorder_command()
    try:
        asyncio.run(sess.run_command(cls, ["-T", "SUSE:Maintenance:2:1"]))
    finally:
        Command.registry.pop(cls.command, None)
    assert seen == ["SUSE:Maintenance:2:1"]


def test_unload_removes_only_the_named_template(tmp_path: Path) -> None:
    """``unload <rrid>`` runs once and removes exactly that template.

    Regression: ``unload`` has ``scope="single"``, so even with several
    templates loaded under MCP (where ``scope="active"`` defaults to fan-out)
    it must not fan out and try to remove the same RRID once per template,
    which failed the second pass with ``TemplateNotLoadedError``.
    """
    from mtui.commands.unload import Unload

    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    out = asyncio.run(sess.run_command(Unload, ["SUSE:Maintenance:1:1"]))

    assert out == ""
    assert sess.templates.rrids() == ["SUSE:Maintenance:2:1"]


def _fake_update(rrid: str) -> MagicMock:
    """Build a fake OBS update whose ``make_testreport`` yields a report.

    The returned report mock carries an ``id`` (str-able to ``rrid``), a
    ``targets`` namespace exposing a settable ``interactive`` flag, and a
    no-op ``autoconnect``.
    """
    targets = MagicMock()
    targets.interactive = True
    report = MagicMock()
    report.id = rrid
    report.targets = targets
    update = MagicMock()
    update.make_testreport.return_value = report
    return update


def test_load_update_does_not_move_active_pointer(tmp_path: Path) -> None:
    """A second ``load_update`` must leave the active pointer on the first load.

    The active template is REPL-only navigation state; over MCP it must not
    move as a side effect of loading. The registry's ``active`` falls back to
    the first-loaded report, so it stays put across subsequent loads.
    """
    sess = _make_session(tmp_path)

    sess.load_update(_fake_update("SUSE:Maintenance:1:1"), autoconnect=False)
    # First load is addressable as the active fallback.
    assert str(sess.templates.active.id) == "SUSE:Maintenance:1:1"

    sess.load_update(_fake_update("SUSE:Maintenance:2:1"), autoconnect=False)
    # Both loaded, but active is still the first — not the last-loaded.
    assert sess.templates.rrids() == [
        "SUSE:Maintenance:1:1",
        "SUSE:Maintenance:2:1",
    ]
    assert str(sess.templates.active.id) == "SUSE:Maintenance:1:1"


def test_load_update_sets_headless_on_freshly_loaded_report(tmp_path: Path) -> None:
    """``interactive=False`` lands on the just-loaded report, not the active one.

    With the active pointer no longer moved on load, the headless flag must be
    applied to the newly loaded report's own host group, even when a different
    (first-loaded) template remains active.
    """
    sess = _make_session(tmp_path)

    first = _fake_update("SUSE:Maintenance:1:1")
    sess.load_update(first, autoconnect=False)
    second = _fake_update("SUSE:Maintenance:2:1")
    sess.load_update(second, autoconnect=False)

    # The second (non-active) report still got the headless flag applied.
    second_report = second.make_testreport.return_value
    assert second_report.targets.interactive is False
    second_report.autoconnect.assert_called_once()


def test_unload_unknown_rrid_raises(tmp_path: Path) -> None:
    """``unload`` on an unloaded RRID surfaces a clean command error."""
    from mtui.commands.unload import Unload
    from mtui.mcp.session import McpCommandError

    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    with pytest.raises(McpCommandError):
        asyncio.run(sess.run_command(Unload, ["SUSE:Maintenance:9:9"]))
    # Both templates remain loaded after a failed unload.
    assert sess.templates.rrids() == [
        "SUSE:Maintenance:1:1",
        "SUSE:Maintenance:2:1",
    ]
