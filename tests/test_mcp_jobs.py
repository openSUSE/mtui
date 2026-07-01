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
        # Cancel the first (blocking) job with a short grace: its body does
        # not poll the cancel event, so the grace expires and the job reports
        # 'cancelling'; release so its thread unwinds to the terminal state.
        await sess.job_cancel(first, grace=0.1)
        release.set()
        await sess._jobs[first]["task"]
        # The second job (its own template lock) still completes.
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
    """``request_review --repost`` must not fan out one ``-T`` job per template.

    Each minted job resolves to exactly one template and would pass the
    command's own multi-template refusal, reposting (and orphaning) every
    template's live review thread — so the fan-out itself must refuse.
    """
    import contextlib

    from mtui.commands.request_review import RequestReview

    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    async def refused() -> None:
        with pytest.raises(McpCommandError, match="scope it with template"):
            await sess.start_jobs(RequestReview, ["--repost"])

    async def scoped() -> list[str]:
        ids = await sess.start_jobs(
            RequestReview, ["--repost", "-T", "SUSE:Maintenance:2:1"]
        )
        # Exactly one job was minted; cancel it before its runner gets a loop
        # slice so the real request_review body never executes in the test.
        task = sess._jobs[ids[0]]["task"]
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task
        return ids

    asyncio.run(refused())
    # An explicitly scoped repost still passes the guard: exactly one job.
    ids = asyncio.run(scoped())
    assert len(ids) == 1


def test_start_jobs_repost_token_in_other_command_fans_out(tmp_path: Path) -> None:
    """The ``--repost`` fan-out guard is ``request_review``-specific.

    Any other backgrounded command whose argv happens to carry the literal
    token (e.g. a ``run`` remote command line) must still fan out normally.
    """
    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    class _RepostTokenProbe(Command):
        command = "repost_token_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            parser.add_argument("--repost", action="store_true")
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            pass

    async def driver() -> list[str]:
        ids = await sess.start_jobs(_RepostTokenProbe, ["--repost"])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(_RepostTokenProbe.command, None)

    assert len(ids) == 2
    assert all(sess.job_status(jid)["state"] == "done" for jid in ids)


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


# --------------------------------------------------------------------------- #
# Truthful cancellation (cooperative-first job_cancel)                         #
# --------------------------------------------------------------------------- #


def _slow_step_probe(name: str):
    """Build a probe whose body blocks inside a simulated side-effecting step.

    Returns ``(cls, entered, release, body_exited)``: the body sets
    ``entered`` on start, blocks on ``release`` (it deliberately does NOT
    poll the cancel event, like a body mid-HTTP/svn step), and sets
    ``body_exited`` on the way out. Caller must unregister ``cls``.
    """
    import threading

    entered = threading.Event()
    release = threading.Event()
    body_exited = threading.Event()

    class _SlowStepProbe(Command):
        command = name

        @classmethod
        def _add_arguments(cls, parser) -> None:
            pass

        def __call__(self) -> None:
            entered.set()
            release.wait(timeout=5)
            body_exited.set()

    return _SlowStepProbe, entered, release, body_exited


def test_job_cancel_waits_for_body_to_stop(tmp_path: Path) -> None:
    """``job_cancel`` reports 'cancelled' only once the worker thread exited.

    While the body is inside a side-effecting step the cancel call must not
    return (the job shows 'cancelling'); only after the thread unwinds does
    it report the job cancelled.
    """
    sess = _make_session(tmp_path)
    cls, entered, release, body_exited = _slow_step_probe("slow_step_probe_tmp")

    async def driver() -> tuple[str, str, bool]:
        job_id = await sess.start_job(cls, [])
        while not entered.is_set():
            await asyncio.sleep(0.01)
        cancel_task = asyncio.create_task(sess.job_cancel(job_id, grace=5))
        await asyncio.sleep(0.1)
        # Body mid-step: no 'cancelled' report yet, state is honest.
        assert not cancel_task.done()
        assert sess.job_status(job_id)["state"] == "cancelling"
        release.set()
        msg = await cancel_task
        return job_id, msg, body_exited.is_set()

    try:
        job_id, msg, exited_before_report = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    # The body had fully exited before job_cancel reported 'cancelled'.
    assert exited_before_report is True
    assert msg == f"cancelled job {job_id}"
    assert sess.job_status(job_id)["state"] == "cancelled"


def test_job_cancel_grace_expiry_reports_cancelling(tmp_path: Path) -> None:
    """An expired grace reports 'cancelling', never a false 'cancelled'."""
    sess = _make_session(tmp_path)
    cls, entered, release, _exited = _slow_step_probe("grace_expiry_probe_tmp")

    async def driver() -> str:
        job_id = await sess.start_job(cls, [])
        while not entered.is_set():
            await asyncio.sleep(0.01)
        msg = await sess.job_cancel(job_id, grace=0.05)
        # The body is still running: the report must not claim it stopped.
        assert "cancelled job" not in msg
        assert "cancelling" in msg
        assert sess.job_status(job_id)["state"] == "cancelling"
        with pytest.raises(McpCommandError, match="still cancelling"):
            sess.job_result(job_id)
        release.set()
        await sess._jobs[job_id]["task"]
        return job_id

    try:
        job_id = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    # Once the thread actually exited the state flips to the terminal one.
    assert sess.job_status(job_id)["state"] == "cancelled"


def test_job_cancel_report_stays_truthful_when_body_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A body that dies (not cancels) during the grace wait is not miscalled.

    The runner's defensive branch records ``failed`` when a
    non-``McpCommandError`` escapes ``_run_sync``; a ``job_cancel`` whose
    grace wait sees that terminal state must report the stop without
    claiming the job was cancelled.
    """
    import threading

    sess = _make_session(tmp_path)
    entered = threading.Event()
    release = threading.Event()

    def _boom(cmd_cls: type[Command], argv: list[str]) -> str:
        entered.set()
        release.wait(timeout=5)
        raise ValueError("kaboom")

    monkeypatch.setattr(sess, "_run_sync", _boom)

    async def driver() -> tuple[str, str]:
        job_id = await sess.start_job(Whoami, [])
        while not entered.is_set():
            await asyncio.sleep(0.01)
        cancel_task = asyncio.create_task(sess.job_cancel(job_id, grace=5))
        await asyncio.sleep(0.05)
        release.set()
        msg = await cancel_task
        return job_id, msg

    job_id, msg = asyncio.run(driver())
    # The body has stopped (no-further-mutations holds), but it failed
    # rather than cancelled — the report must not claim otherwise.
    assert "cancelled job" not in msg
    assert "state=failed" in msg
    assert sess.job_status(job_id)["state"] == "failed"


def test_same_template_rerun_serialises_until_cancelled_thread_exits(
    tmp_path: Path,
) -> None:
    """A re-run after a grace-expired cancel queues behind the old thread.

    The cancelled job's runner keeps holding the template's per-RRID lock
    until its worker thread really exits, so the documented cancel-then-rerun
    flow serialises instead of interleaving (no stale-thread Slack/svn writes
    racing the new run).
    """
    import threading

    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    rrid = "SUSE:Maintenance:1:1"

    first_entered = threading.Event()
    second_entered = threading.Event()
    release = threading.Event()
    order: list[str] = []

    class _RerunProbe(Command):
        command = "rerun_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            # Bodies on one template serialise on its per-RRID lock, so the
            # shared list is safe: first invocation blocks, second records.
            if not order:
                order.append("first")
                first_entered.set()
                release.wait(timeout=5)
                order.append("first_exit")
            else:
                order.append("second")
                second_entered.set()

    async def driver() -> tuple[str, str]:
        first = (await sess.start_jobs(_RerunProbe, ["-T", rrid]))[0]
        while not first_entered.is_set():
            await asyncio.sleep(0.01)
        await sess.job_cancel(first, grace=0.05)
        assert sess.job_status(first)["state"] == "cancelling"
        # Re-run on the same template while the old thread is still alive.
        second = (await sess.start_jobs(_RerunProbe, ["-T", rrid]))[0]
        await asyncio.sleep(0.1)
        # The new body must not have started: it is queued on the lock the
        # cancelled-but-still-running job keeps holding.
        assert not second_entered.is_set()
        assert sess.job_status(second)["state"] == "running"
        release.set()
        await sess._jobs[first]["task"]
        await sess._jobs[second]["task"]
        return first, second

    try:
        first, second = asyncio.run(driver())
    finally:
        Command.registry.pop(_RerunProbe.command, None)

    assert order == ["first", "first_exit", "second"]
    assert sess.job_status(first)["state"] == "cancelled"
    assert sess.job_status(second)["state"] == "done"


def test_job_cancel_queued_job_never_starts_body(tmp_path: Path) -> None:
    """Cancelling a job still queued on its template lock is immediate.

    The body provably never ran (the pre-start guard refuses once the cancel
    event is flagged), so the hard-cancel path may report 'cancelled' at once
    — even after the lock frees, the cancelled job's body stays unrun.
    """
    import threading

    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    rrid = "SUSE:Maintenance:1:1"

    entered = threading.Event()
    release = threading.Event()
    calls: list[str] = []

    class _QueueProbe(Command):
        command = "queue_probe_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            calls.append("body")
            if len(calls) == 1:
                entered.set()
                release.wait(timeout=5)

    async def driver() -> tuple[str, str, str]:
        first = (await sess.start_jobs(_QueueProbe, ["-T", rrid]))[0]
        while not entered.is_set():
            await asyncio.sleep(0.01)
        second = (await sess.start_jobs(_QueueProbe, ["-T", rrid]))[0]
        await asyncio.sleep(0.05)
        # ``second`` is queued (its body never started): the cancel resolves
        # immediately and truthfully, no grace involved.
        msg = await sess.job_cancel(second)
        release.set()
        await sess._jobs[first]["task"]
        return first, second, msg

    try:
        first, second, msg = asyncio.run(driver())
    finally:
        Command.registry.pop(_QueueProbe.command, None)

    assert msg == f"cancelled job {second}"
    assert sess.job_status(second)["state"] == "cancelled"
    assert sess.job_status(first)["state"] == "done"
    # The cancelled job's body never ran, even after the lock freed.
    assert calls == ["body"]


# --------------------------------------------------------------------------- #
# Foreground multi-template watch guard                                        #
# --------------------------------------------------------------------------- #


def test_foreground_multi_template_watch_refused(tmp_path: Path) -> None:
    """``run_command`` refuses a foreground watching multi-template review.

    An unscoped watch-bearing ``request_review`` would hold the registry's
    exclusive gate for the whole (multi-hour) watch, freezing every other
    tool call in the MCP session; the caller is pointed at background=true /
    template scoping / no_watch instead. Post-only, scoped and unrelated
    commands pass the guard.
    """
    from mtui.commands.request_review import RequestReview

    sess = _make_session(tmp_path)
    _load_two_reports(sess)

    with pytest.raises(McpCommandError, match="background=true"):
        asyncio.run(sess.run_command(RequestReview, []))

    # Narrow guard: none of these raise (post-only, scoped, other command).
    sess._refuse_foreground_multi_watch(RequestReview, ["--no-watch"])
    sess._refuse_foreground_multi_watch(RequestReview, ["-T", "SUSE:Maintenance:2:1"])
    sess._refuse_foreground_multi_watch(Whoami, [])
