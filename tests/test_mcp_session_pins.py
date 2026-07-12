"""Mutation-killing pinning tests for :mod:`mtui.mcp.session`.

A full mutmut run left surviving mutants in code the suite executes but
never asserts on: the background-job bookkeeping (``_mint_job`` /
``job_result`` / ``_job_view`` / ``job_cancel``), the ``SystemExit``
conversion and error envelopes in ``_run_sync``, the ``load_update``
forwarding contract, and ``_FakeSys``. These tests pin the observable
behaviour those mutants change, so equivalent edits fail loudly.

Style matches ``test_mcp_session.py`` / ``test_mcp_jobs.py``: MagicMock
config, ``asyncio.run`` drivers (no pytest-asyncio), throwaway probe
commands unregistered in ``finally``.
"""

from __future__ import annotations

import asyncio
import io
import logging
import threading
import time
from pathlib import Path
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from mtui.commands import Command
from mtui.commands.whoami import Whoami
from mtui.mcp.session import McpCommandError, McpSession, _FakeSys


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _make_session(tmp_path: Path) -> McpSession:
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.pins"))


def _load_two_reports(sess: McpSession) -> None:
    """Add two MagicMock reports so a fan-out command resolves to both."""
    for rrid in ("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"):
        report = MagicMock()
        report.id = rrid
        report.targets = {}
        sess.templates.add(report)
    sess.templates.set_active("SUSE:Maintenance:1:1")


# --------------------------------------------------------------------------- #
# _FakeSys                                                                    #
# --------------------------------------------------------------------------- #


def test_fakesys_surface() -> None:
    """``_FakeSys`` exposes argv, fresh streams and a raising ``exit``."""
    fs = _FakeSys()
    assert fs.argv == ["mtui-mcp"]
    assert isinstance(fs.stdout, io.StringIO)
    assert isinstance(fs.stderr, io.StringIO)
    assert fs.stdout.getvalue() == ""
    assert fs.stderr.getvalue() == ""
    with pytest.raises(SystemExit) as ei:
        fs.exit(5)
    assert ei.value.code == 5
    with pytest.raises(SystemExit) as ei:
        fs.exit()
    assert ei.value.code == 0


# --------------------------------------------------------------------------- #
# _run_sync: SystemExit conversion                                            #
# --------------------------------------------------------------------------- #


def _exit_probe():
    """Probe that prints, writes stderr, then ``self.sys.exit(<code>)``."""

    class _ExitProbe(Command):
        command = "_mcp_pin_exit_probe"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            parser.add_argument("code")

        def __call__(self) -> None:
            self.println("ran before exit")
            self.sys.stderr.write("exit-probe complaint\n")
            self.sys.exit(int(self.args.code))

    return _ExitProbe


def test_sys_exit_nonzero_carries_streams_and_exact_code(tmp_path: Path) -> None:
    """``sys.exit(3)`` becomes McpCommandError with the captured streams."""
    sess = _make_session(tmp_path)
    cls = _exit_probe()
    try:
        with pytest.raises(McpCommandError) as ei:
            asyncio.run(sess.run_command(cls, ["3"]))
    finally:
        Command.registry.pop(cls.command, None)

    assert ei.value.exit_code == 3
    assert ei.value.stdout == "ran before exit\n"
    assert ei.value.stderr == "exit-probe complaint\n"
    assert str(ei.value) == "command failed (exit_code=3): exit-probe complaint"


def test_sys_exit_one_still_raises(tmp_path: Path) -> None:
    """``sys.exit(1)`` is a failure too (the check is ``!= 0``, not ``!= 1``)."""
    sess = _make_session(tmp_path)
    cls = _exit_probe()
    try:
        with pytest.raises(McpCommandError) as ei:
            asyncio.run(sess.run_command(cls, ["1"]))
    finally:
        Command.registry.pop(cls.command, None)
    assert ei.value.exit_code == 1


def test_sys_exit_zero_returns_stdout_and_warns_about_stderr(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """``sys.exit(0)`` is a clean return; leftover stderr goes to the log."""
    sess = _make_session(tmp_path)
    cls = _exit_probe()
    try:
        with caplog.at_level(logging.WARNING, logger="test.mcp.pins"):
            out = asyncio.run(sess.run_command(cls, ["0"]))
    finally:
        Command.registry.pop(cls.command, None)

    assert out == "ran before exit\n"
    messages = [r.getMessage() for r in caplog.records]
    assert any(
        "wrote to stderr" in m and cls.command in m and "exit-probe complaint" in m
        for m in messages
    ), messages


def test_systemexit_none_is_a_clean_return(tmp_path: Path) -> None:
    """``SystemExit(None)`` maps to code 0 -> the command's stdout is returned."""
    sess = _make_session(tmp_path)

    class _ExitNoneProbe(Command):
        command = "_mcp_pin_exit_none_probe"

        def __call__(self) -> None:
            self.println("survived exit(None)")
            raise SystemExit(None)

    try:
        out = asyncio.run(sess.run_command(_ExitNoneProbe, []))
    finally:
        Command.registry.pop(_ExitNoneProbe.command, None)
    assert out == "survived exit(None)\n"


def test_systemexit_non_int_maps_to_exit_code_one(tmp_path: Path) -> None:
    """``SystemExit('msg')`` (non-int code) is reported as exit_code 1."""
    sess = _make_session(tmp_path)

    class _ExitStrProbe(Command):
        command = "_mcp_pin_exit_str_probe"

        def __call__(self) -> None:
            raise SystemExit("catastrophe")

    try:
        with pytest.raises(McpCommandError) as ei:
            asyncio.run(sess.run_command(_ExitStrProbe, []))
    finally:
        Command.registry.pop(_ExitStrProbe.command, None)
    assert ei.value.exit_code == 1


# --------------------------------------------------------------------------- #
# _run_sync: error envelopes and output plumbing                              #
# --------------------------------------------------------------------------- #


def test_generic_exception_envelope_carries_stdout_and_repr(tmp_path: Path) -> None:
    """An unhandled exception yields exit 1, the partial stdout and repr(exc)."""
    sess = _make_session(tmp_path)

    class _RaisingProbe(Command):
        command = "_mcp_pin_raising_probe"

        def __call__(self) -> None:
            self.println("printed before crash")
            raise RuntimeError("boom")

    try:
        with pytest.raises(McpCommandError) as ei:
            asyncio.run(sess.run_command(_RaisingProbe, []))
    finally:
        Command.registry.pop(_RaisingProbe.command, None)

    assert ei.value.exit_code == 1
    assert ei.value.stdout == "printed before crash\n"
    # No stderr was written, so the envelope falls back to repr(exc).
    assert ei.value.stderr == "RuntimeError('boom')"
    assert str(ei.value) == "command failed (exit_code=1): RuntimeError('boom')"


def test_generic_exception_prefers_written_stderr_over_repr(tmp_path: Path) -> None:
    """When the command wrote to stderr before crashing, that text wins."""
    sess = _make_session(tmp_path)

    class _StderrThenRaiseProbe(Command):
        command = "_mcp_pin_stderr_then_raise_probe"

        def __call__(self) -> None:
            self.sys.stderr.write("detailed diagnosis\n")
            raise RuntimeError("boom")

    try:
        with pytest.raises(McpCommandError) as ei:
            asyncio.run(sess.run_command(_StderrThenRaiseProbe, []))
    finally:
        Command.registry.pop(_StderrThenRaiseProbe.command, None)

    assert ei.value.stderr == "detailed diagnosis\n"
    assert "RuntimeError" not in ei.value.stderr


def test_argparse_failure_envelope_fields(tmp_path: Path) -> None:
    """An unknown flag yields exit 2, the usage on stdout, the complaint on stderr."""
    sess = _make_session(tmp_path)
    with pytest.raises(McpCommandError) as ei:
        asyncio.run(sess.run_command(Whoami, ["--bogus"]))
    assert ei.value.exit_code == 2
    # mtui's ArgumentParser routes print_usage to the captured stdout.
    assert ei.value.stdout == "usage: whoami [-h]\n"
    assert "bogus" in ei.value.stderr


def test_argparse_help_exit_carries_help_text_in_stdout(tmp_path: Path) -> None:
    """``--help`` (argparse exits 0) maps to exit 2 with the help on stdout.

    Pins the ``e.status or 2`` fallback and that the parse-failure envelope
    really carries the captured stdout (help goes to the fake stdout).
    """
    sess = _make_session(tmp_path)
    with pytest.raises(McpCommandError) as ei:
        asyncio.run(sess.run_command(Whoami, ["--help"]))
    assert ei.value.exit_code == 2
    assert "usage:" in ei.value.stdout
    assert "whoami" in ei.value.stdout


def test_prompt_println_lands_in_captured_output(tmp_path: Path) -> None:
    """``self.prompt.println`` writes into the per-call stdout buffer."""
    sess = _make_session(tmp_path)

    class _PromptPrintlnProbe(Command):
        command = "_mcp_pin_prompt_println_probe"

        def __call__(self) -> None:
            self.prompt.println("routed via session println")

    try:
        out = asyncio.run(sess.run_command(_PromptPrintlnProbe, []))
    finally:
        Command.registry.pop(_PromptPrintlnProbe.command, None)
    assert out == "routed via session println\n"


def test_println_outside_a_call_falls_back_to_warning_log(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """With no command in flight, ``println`` logs the text at WARNING."""
    sess = _make_session(tmp_path)
    with caplog.at_level(logging.WARNING, logger="test.mcp.pins"):
        sess.println("orphan line")
    assert any(r.getMessage() == "orphan line" for r in caplog.records)


def test_log_capture_is_scoped_to_the_mtui_tree(tmp_path: Path) -> None:
    """Only records logged under the ``mtui`` logger are teed into the reply."""
    sess = _make_session(tmp_path)

    class _TreeProbe(Command):
        command = "_mcp_pin_log_tree_probe"

        def __call__(self) -> None:
            logging.getLogger("mtui.mcp_pins.inside").warning("inside mtui tree")
            logging.getLogger("mcp_pins.outside").warning("outside mtui tree")
            self.println("tree probe done")

    try:
        out = asyncio.run(sess.run_command(_TreeProbe, []))
    finally:
        Command.registry.pop(_TreeProbe.command, None)

    assert "WARNING: inside mtui tree" in out
    assert "outside mtui tree" not in out
    assert "tree probe done" in out


# --------------------------------------------------------------------------- #
# _mint_job / _job_view: the running-job record and view                      #
# --------------------------------------------------------------------------- #


def test_running_job_record_and_view_shape(tmp_path: Path) -> None:
    """A gate-blocked ``start_job`` is observably 'running' with elapsed_s.

    Drives ``job_status`` / ``job_list`` / ``job_result`` against a job minted
    by ``start_job`` itself (not a hand-crafted record), pinning the initial
    record fields and the public view keys.
    """
    sess = _make_session(tmp_path)

    async def driver() -> str:
        async with sess._registry.exclusive():
            job_id = await sess.start_job(Whoami, [])
            # Let the runner task advance to its (blocked) gate acquisition.
            await asyncio.sleep(0)
            await asyncio.sleep(0)

            # The raw record was minted with the documented initial fields.
            rec = sess._jobs[job_id]
            assert rec["id"] == job_id
            assert rec["command"] == "whoami"
            assert rec["argv"] == []
            assert rec["state"] == "running"
            assert isinstance(rec["started"], float)
            assert rec["finished"] is None
            assert rec["result"] is None
            assert rec["error"] is None
            assert rec["exit_code"] is None
            assert rec["task"] is not None

            # The public view exposes exactly id/command/state/elapsed_s.
            view = sess.job_status(job_id)
            assert set(view) == {"id", "command", "state", "elapsed_s"}
            assert view["id"] == job_id
            assert view["command"] == "whoami"
            assert view["state"] == "running"
            assert isinstance(view["elapsed_s"], float)
            assert 0.0 <= view["elapsed_s"] < 60.0

            # job_list shows the same running job.
            listed = {v["id"]: v for v in sess.job_list()}
            assert listed[job_id]["state"] == "running"

            # job_result on a running job points the caller at job_status.
            with pytest.raises(McpCommandError) as ei:
                sess.job_result(job_id)
            assert "still running" in ei.value.stderr
            assert "poll job_status" in ei.value.stderr
            assert ei.value.stdout == ""
            assert ei.value.exit_code == 1
        await sess._jobs[job_id]["task"]
        return job_id

    job_id = asyncio.run(driver())
    assert sess.job_status(job_id)["state"] == "done"


def test_finished_job_elapsed_uses_the_finished_timestamp(tmp_path: Path) -> None:
    """A done job's elapsed_s is frozen at finished-started, not still ticking."""
    sess = _make_session(tmp_path)

    async def driver() -> str:
        job_id = await sess.start_job(Whoami, [])
        await sess._jobs[job_id]["task"]
        return job_id

    job_id = asyncio.run(driver())
    rec = sess._jobs[job_id]
    assert isinstance(rec["finished"], float)
    expected = round(rec["finished"] - rec["started"], 1)

    first = sess.job_status(job_id)["elapsed_s"]
    time.sleep(0.3)
    second = sess.job_status(job_id)["elapsed_s"]
    assert first == expected
    assert second == expected


# --------------------------------------------------------------------------- #
# _mint_job / job_result: failure bookkeeping                                 #
# --------------------------------------------------------------------------- #


def test_failed_job_envelope_preserves_streams_and_exit_code(tmp_path: Path) -> None:
    """A failed background job re-raises the exact foreground envelope."""
    sess = _make_session(tmp_path)

    class _FailingJobProbe(Command):
        command = "_mcp_pin_failing_job_probe"

        def __call__(self) -> None:
            self.println("partial result")
            self.sys.stderr.write("wrote to stderr before dying")
            self.sys.exit(3)

    async def driver() -> str:
        job_id = await sess.start_job(_FailingJobProbe, [])
        await sess._jobs[job_id]["task"]
        return job_id

    try:
        job_id = asyncio.run(driver())
    finally:
        Command.registry.pop(_FailingJobProbe.command, None)

    assert sess.job_status(job_id)["state"] == "failed"
    rec = sess._jobs[job_id]
    assert rec["exit_code"] == 3
    assert rec["result"] == "partial result\n"
    assert rec["error"] == "command failed (exit_code=3): wrote to stderr before dying"

    with pytest.raises(McpCommandError) as ei:
        sess.job_result(job_id)
    assert ei.value.exit_code == 3
    assert ei.value.stdout == "partial result\n"
    assert ei.value.stderr == (
        "command failed (exit_code=3): wrote to stderr before dying"
    )


def test_job_runner_generic_exception_records_repr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-McpCommandError escaping the runner is recorded as repr(exc).

    ``_run_sync`` normally wraps everything; patching it to raise directly
    exercises ``_mint_job``'s defensive generic-exception branch (state
    'failed', error=repr, exit_code left None -> job_result falls back to 1).
    """
    sess = _make_session(tmp_path)

    def _boom(cmd_cls, argv):
        raise ValueError("kaboom")

    monkeypatch.setattr(sess, "_run_sync", _boom)

    async def driver() -> str:
        job_id = await sess.start_job(Whoami, [])
        await sess._jobs[job_id]["task"]
        return job_id

    job_id = asyncio.run(driver())
    rec = sess._jobs[job_id]
    assert rec["state"] == "failed"
    assert rec["error"] == "ValueError('kaboom')"
    assert rec["exit_code"] is None
    assert isinstance(rec["finished"], float)

    with pytest.raises(McpCommandError) as ei:
        sess.job_result(job_id)
    assert ei.value.exit_code == 1
    assert ei.value.stdout == ""
    assert ei.value.stderr == "ValueError('kaboom')"


# --------------------------------------------------------------------------- #
# job_cancel / job_result: the cancelled branch                               #
# --------------------------------------------------------------------------- #


def test_job_cancel_itself_flips_state_and_job_result_reports_it(
    tmp_path: Path,
) -> None:
    """``job_cancel`` cancels the worker task and the state flips immediately.

    The state is asserted right after ``job_cancel`` returns, while the gate
    that blocked the job is still held — so it can only have been set by the
    cancellation ``job_cancel`` performed, never by event-loop teardown.
    """
    sess = _make_session(tmp_path)

    async def driver() -> str:
        async with sess._registry.exclusive():
            job_id = await sess.start_job(Whoami, [])
            await asyncio.sleep(0)
            await asyncio.sleep(0)

            msg = await sess.job_cancel(job_id)
            assert msg == f"cancelled job {job_id}"
            # Immediately observable — before the gate is released.
            assert sess.job_status(job_id)["state"] == "cancelled"
            assert sess._jobs[job_id]["finished"] is not None

            with pytest.raises(McpCommandError) as ei:
                sess.job_result(job_id)
            assert f"job {job_id} was cancelled" in ei.value.stderr
            assert ei.value.exit_code == 1
            assert ei.value.stdout == ""
        return job_id

    job_id = asyncio.run(driver())
    # The cancelled outcome is stable after the loop is gone, too.
    assert sess.job_status(job_id)["state"] == "cancelled"


def test_job_cancel_on_a_finished_job_is_a_noop(tmp_path: Path) -> None:
    """Cancelling a done job leaves it done and still returns the message."""
    sess = _make_session(tmp_path)

    async def driver() -> tuple[str, str]:
        job_id = await sess.start_job(Whoami, [])
        await sess._jobs[job_id]["task"]
        msg = await sess.job_cancel(job_id)
        return job_id, msg

    job_id, msg = asyncio.run(driver())
    assert msg == f"cancelled job {job_id}"
    assert sess.job_status(job_id)["state"] == "done"


# --------------------------------------------------------------------------- #
# _mint_job: per-template locking of background jobs                          #
# --------------------------------------------------------------------------- #


def test_background_jobs_on_different_templates_overlap(tmp_path: Path) -> None:
    """Two fanned-out jobs on different RRIDs run concurrently, not serially.

    Each job body waits on a shared two-party barrier: only if both bodies
    are in flight at the same time (per-RRID shared locking) can the barrier
    release. A degradation to the exclusive gate would serialise them and
    break the barrier instead.
    """
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    barrier = threading.Barrier(2)

    class _BarrierJobProbe(Command):
        command = "_mcp_pin_barrier_job_probe"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            barrier.wait(timeout=10)
            self.println(str(self.metadata.id))

    async def driver() -> list[str]:
        ids = await sess.start_jobs(_BarrierJobProbe, [])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(_BarrierJobProbe.command, None)

    assert len(ids) == 2
    assert all(sess.job_status(jid)["state"] == "done" for jid in ids)
    # Each job ran scoped to its own template and recorded the scoped argv.
    expected_rrids = {"SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"}
    seen: set[str] = set()
    for jid in ids:
        rrid = sess.job_result(jid).strip()
        seen.add(rrid)
        assert sess._jobs[jid]["argv"] == ["-T", rrid]
    assert seen == expected_rrids


# --------------------------------------------------------------------------- #
# load_update: headless forwarding contract                                   #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("autoconnect", [True, False])
def test_load_update_forwards_config_and_headless_flags(
    tmp_path: Path, autoconnect: bool
) -> None:
    """``make_testreport`` gets the session config, autoconnect, and the
    headless contract (interactive=False, prompter=None)."""
    sess = _make_session(tmp_path)

    targets = MagicMock()
    targets.interactive = True
    report = MagicMock()
    report.id = "SUSE:Maintenance:1:1"
    report.targets = targets
    update = MagicMock()
    update.make_testreport.return_value = report

    sess.load_update(update, autoconnect=autoconnect)

    update.make_testreport.assert_called_once_with(
        sess.config, autoconnect, False, prompter=None
    )
