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
from unittest.mock import MagicMock

import pytest

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
                async with sess._lock:
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
