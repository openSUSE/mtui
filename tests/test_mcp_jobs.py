"""Tests for the background-job (async slow-op) path in :mod:`mtui.mcp.session`.

A backgrounded command runs in an asyncio task that holds the session lock
for its duration and records its outcome on the job table; ``job_status`` /
``job_result`` read that table. These tests drive the lifecycle with the
real ``whoami`` command (fast, no hosts) and ``asyncio.run`` wrappers,
matching the style of ``test_mcp_session.py``.
"""

from __future__ import annotations

import asyncio
import logging
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
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.jobs"))


def _load_two_reports(sess: McpSession) -> None:
    """Add two MagicMock reports so a fan-out command resolves to both."""
    for rrid in ("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"):
        report = MagicMock()
        report.id = rrid
        report.targets = {}
        sess.templates.add(report)
    sess.templates.set_active("SUSE:Maintenance:1:1")


def _fanout_probe():
    """Build a throwaway fast fan-out command; caller must unregister it."""

    class _FanoutJobProbe(Command):
        command = "fanout_job_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            self.println(str(self.metadata.id))

    return _FanoutJobProbe


def test_start_job_runs_and_result_returns_stdout(tmp_path: Path) -> None:
    """A backgrounded command finishes 'done' and yields its stdout."""
    sess = _make_session(tmp_path)

    async def driver() -> tuple[str, dict, str]:
        job_id = await sess.start_job(Whoami, [])
        await sess._jobs[job_id]["task"]  # let the worker finish
        return job_id, sess.job_status(job_id), sess.job_result(job_id)

    job_id, status, result = asyncio.run(driver())
    assert job_id.startswith("whoami-")
    assert status["state"] == "done"
    assert status["command"] == "whoami"
    assert result.startswith("User: testuser, app pid: ")


def test_job_result_failed_surfaces_error_envelope(tmp_path: Path) -> None:
    """A job whose command fails records 'failed'; job_result raises."""
    sess = _make_session(tmp_path)

    async def driver() -> str:
        job_id = await sess.start_job(Whoami, ["--nonexistent-flag"])
        await sess._jobs[job_id]["task"]
        return job_id

    job_id = asyncio.run(driver())
    assert sess.job_status(job_id)["state"] == "failed"
    with pytest.raises(McpCommandError):
        sess.job_result(job_id)


def test_job_result_running_tells_caller_to_poll(tmp_path: Path) -> None:
    """job_result on a still-running job raises, pointing at job_status."""
    sess = _make_session(tmp_path)
    gate = asyncio.Event()

    async def driver() -> None:
        # A job whose body blocks until we release the gate.
        async def _blocker() -> None:
            try:
                async with sess._registry.exclusive():
                    await gate.wait()
            except asyncio.CancelledError:
                raise

        job_id = "blocker-1"
        sess._jobs[job_id] = {
            "id": job_id,
            "command": "blocker",
            "argv": [],
            "state": "running",
            "started": 0.0,
            "finished": None,
            "result": None,
            "error": None,
            "exit_code": None,
            "task": asyncio.create_task(_blocker()),
        }
        with pytest.raises(McpCommandError, match="still running"):
            sess.job_result(job_id)
        gate.set()
        sess._jobs[job_id]["task"].cancel()

    asyncio.run(driver())


def test_job_status_unknown_id_raises(tmp_path: Path) -> None:
    """Querying an unknown job id raises a clean error."""
    sess = _make_session(tmp_path)
    with pytest.raises(McpCommandError, match="no such job"):
        sess.job_status("nope-1")


def test_job_list_reports_started_jobs(tmp_path: Path) -> None:
    """job_list enumerates every started job with its state."""
    sess = _make_session(tmp_path)

    async def driver() -> list[dict]:
        a = await sess.start_job(Whoami, [])
        b = await sess.start_job(Whoami, [])
        await sess._jobs[a]["task"]
        await sess._jobs[b]["task"]
        return sess.job_list()

    jobs = asyncio.run(driver())
    assert len(jobs) == 2
    assert all(j["state"] == "done" for j in jobs)


def test_job_cancel_unknown_id_raises(tmp_path: Path) -> None:
    """Cancelling an unknown job id raises a clean error."""
    sess = _make_session(tmp_path)
    with pytest.raises(McpCommandError, match="no such job"):
        asyncio.run(sess.job_cancel("nope-1"))


# --------------------------------------------------------------------------- #
# Per-template background jobs (Phase 4)                                       #
# --------------------------------------------------------------------------- #


def test_start_jobs_single_template_keeps_one_job(tmp_path: Path) -> None:
    """With no fan-out, ``start_jobs`` mints one job with the legacy id shape."""
    sess = _make_session(tmp_path)

    async def driver() -> list[str]:
        ids = await sess.start_jobs(Whoami, [])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    ids = asyncio.run(driver())
    assert len(ids) == 1
    assert ids[0].startswith("whoami-")
    assert "SUSE" not in ids[0]


def test_start_jobs_fans_out_one_job_per_template(tmp_path: Path) -> None:
    """A fanned-out slow command mints one job per loaded template."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls = _fanout_probe()

    async def driver() -> list[str]:
        ids = await sess.start_jobs(cls, [])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    assert len(ids) == 2
    # ids encode the (sanitised) RRID and are unique.
    assert len(set(ids)) == 2
    assert any("SUSE_Maintenance_1_1" in jid for jid in ids)
    assert any("SUSE_Maintenance_2_1" in jid for jid in ids)
    # job_list shows both, each done, and each scoped to its own RRID.
    listed = sess.job_list()
    assert len(listed) == 2
    assert all(j["state"] == "done" for j in listed)
    outputs = {sess.job_result(jid).strip() for jid in ids}
    assert outputs == {"SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"}


def test_start_jobs_explicit_template_yields_single_job(tmp_path: Path) -> None:
    """A client-supplied ``-T`` narrows to one template -> one job."""
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls = _fanout_probe()

    async def driver() -> list[str]:
        ids = await sess.start_jobs(cls, ["-T", "SUSE:Maintenance:2:1"])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    assert len(ids) == 1
    assert sess.job_result(ids[0]).strip() == "SUSE:Maintenance:2:1"


def test_cancel_one_template_job_leaves_others(tmp_path: Path) -> None:
    """Cancelling one per-template job does not abort the sibling jobs."""
    import threading

    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    # The first template's body blocks on this event (set from the driver
    # after the cancel) so we have a deterministic window to cancel it; the
    # second template's body returns at once.
    release = threading.Event()
    blocking_started = threading.Event()

    class _BlockingProbe(Command):
        command = "blocking_job_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            if str(self.metadata.id) == "SUSE:Maintenance:1:1":
                blocking_started.set()
                release.wait(timeout=5)

    async def driver() -> tuple[str, str]:
        ids = await sess.start_jobs(_BlockingProbe, [])
        first, second = ids
        # Wait until the first job's blocking body is actually running.
        while not blocking_started.is_set():
            await asyncio.sleep(0.01)
        # Cancel the first (blocking) job; release so its thread unwinds.
        await sess.job_cancel(first)
        release.set()
        # The second job, queued behind the session lock, still completes.
        await sess._jobs[second]["task"]
        return first, second

    try:
        first, second = asyncio.run(driver())
    finally:
        Command.registry.pop(_BlockingProbe.command, None)

    assert sess.job_status(first)["state"] == "cancelled"
    assert sess.job_status(second)["state"] == "done"
    assert sess.job_result(second).strip() == ""


def test_start_jobs_refuses_unscoped_repost_fanout(tmp_path: Path) -> None:
    """``--repost`` must not be silently fanned out one ``-T`` job per template.

    Each minted job resolves to exactly one template and would pass the
    command's own multi-template refusal, reposting (and orphaning) every
    template's live review thread — so the fan-out itself must refuse.
    """
    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    class _RepostProbe(Command):
        command = "repost_job_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            parser.add_argument("--repost", action="store_true")
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            pass

    async def refused() -> None:
        with pytest.raises(McpCommandError, match="scope it with template"):
            await sess.start_jobs(_RepostProbe, ["--repost"])

    async def scoped() -> list[str]:
        ids = await sess.start_jobs(
            _RepostProbe, ["--repost", "-T", "SUSE:Maintenance:2:1"]
        )
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        asyncio.run(refused())
        # An explicitly scoped repost still works: exactly one job.
        ids = asyncio.run(scoped())
    finally:
        Command.registry.pop(_RepostProbe.command, None)

    assert len(ids) == 1


def test_job_cancel_sets_cooperative_cancel_event(tmp_path: Path) -> None:
    """``job_cancel`` flags the contextvar cancel event so a polling body exits.

    ``request_review``'s Slack watch passes this event into ``wait_for_ack``;
    without it a cancelled job's worker thread would keep watching (and could
    still auto-approve) until its own multi-hour timeout.
    """
    import threading

    from mtui.support.cancellation import current_cancel_event

    sess = _make_session(tmp_path)

    entered = threading.Event()
    finished = threading.Event()
    observed: dict[str, bool] = {}

    class _CancelProbe(Command):
        command = "cancel_probe_tmp"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            pass

        def __call__(self) -> None:
            ev = current_cancel_event.get()
            entered.set()
            # Block like a review watch would, until job_cancel sets the event
            # (bounded so a broken implementation fails the test, not hangs it).
            observed["event_set"] = ev is not None and ev.wait(timeout=5)
            finished.set()

    async def driver() -> str:
        job_id = await sess.start_job(_CancelProbe, [])
        while not entered.is_set():
            await asyncio.sleep(0.01)
        await sess.job_cancel(job_id)
        return job_id

    try:
        job_id = asyncio.run(driver())
    finally:
        Command.registry.pop(_CancelProbe.command, None)

    # The worker thread observed the cancellation promptly (not a timeout).
    assert finished.wait(timeout=5)
    assert observed["event_set"] is True
    assert sess.job_status(job_id)["state"] == "cancelled"
