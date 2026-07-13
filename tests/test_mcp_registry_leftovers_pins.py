"""Mutation-killing pin for the idle-sweeper's re-validation loop.

``tests/test_mcp_registry.py::test_sweep_spares_session_reactivated_during_the_sweep``
already pins that a session re-activated mid-sweep is spared. But in that
test the re-activated key is the *last* entry in the sweep's stale batch,
so the "re-activated -> skip this one" ``continue`` is indistinguishable
from a ``break``: either way the loop has nothing left to do. A full
mutmut run found exactly that survivor (``continue`` -> ``break`` in
``SessionRegistry._sweep_loop``'s re-validation branch).

This test mirrors that fixture but inserts a third, genuinely-idle key
*after* the reactivated one in stale order, so ``continue`` (proceed to
the next stale key) and ``break`` (abandon the rest of this sweep round)
produce observably different outcomes: only ``continue`` still evicts
the trailing idle key in the same round.
"""

from __future__ import annotations

import asyncio
import logging
import time
from pathlib import Path
from unittest.mock import MagicMock

from mtui.mcp.main import build_session
from mtui.mcp.registry import SessionRegistry

_LOG = logging.getLogger("test.mcp.registry.leftovers")


def _config(tmp_path: Path) -> MagicMock:
    """The minimal Config shape McpSession's constructor touches."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _registry(tmp_path: Path, *, idle_timeout: float) -> SessionRegistry:
    return SessionRegistry(
        build_session,
        _config(tmp_path),
        _LOG,
        max_sessions=32,
        idle_timeout=idle_timeout,
    )


def test_sweep_continues_past_a_reactivated_key_to_evict_a_later_idle_one(
    tmp_path: Path,
) -> None:
    """A stale batch of [k1, k2, k3] where k2 is reactivated mid-sweep.

    k1's close is slow (buys the window needed to reactivate k2 before the
    sweep reaches it); k2 is refreshed during that window and must be
    spared (hits the "re-activated" branch); k3 is never touched and must
    still be evicted in the *same* sweep round. A ``continue`` -> ``break``
    mutation on the re-activation branch would stop the round at k2 and
    leave k3 (genuinely idle) un-evicted.
    """
    reg = _registry(tmp_path, idle_timeout=4.0)  # sweep interval = 2s

    close_started = asyncio.Event()
    release_close = asyncio.Event()
    k2_closed = {"n": 0}
    k3_closed = {"n": 0}

    async def driver() -> None:
        s1 = await reg.get_or_create("k1")
        s2 = await reg.get_or_create("k2")
        s3 = await reg.get_or_create("k3")

        async def _slow_close() -> None:
            close_started.set()
            await release_close.wait()

        async def _k2_close() -> None:
            k2_closed["n"] += 1

        async def _k3_close() -> None:
            k3_closed["n"] += 1

        s1.close = _slow_close  # ty: ignore[invalid-assignment]
        s2.close = _k2_close  # ty: ignore[invalid-assignment]
        s3.close = _k3_close  # ty: ignore[invalid-assignment]

        # Age all three well past the TTL so the next sweep round snapshots
        # stale = [k1, k2, k3] (insertion order).
        aged = time.monotonic() - 100
        reg._last_touch["k1"] = aged  # noqa: SLF001
        reg._last_touch["k2"] = aged  # noqa: SLF001
        reg._last_touch["k3"] = aged  # noqa: SLF001

        # Wait until the sweeper is mid-eviction of k1 (blocked in close),
        # then do what a live client does: grab k2, refreshing its touch.
        await asyncio.wait_for(close_started.wait(), timeout=10)
        again = await reg.get_or_create("k2")
        assert again is s2

        # Let the k1 eviction finish; the sweep round proceeds to k2 (spared)
        # and must then continue on to k3 (genuinely idle).
        release_close.set()
        await asyncio.sleep(0.5)

        assert "k1" not in reg._sessions  # noqa: SLF001 -- genuinely idle: reaped
        assert "k2" in reg._sessions  # noqa: SLF001 -- re-activated: spared
        assert k2_closed["n"] == 0
        assert "k3" not in reg._sessions  # noqa: SLF001 -- idle: reaped despite k2's skip
        assert k3_closed["n"] == 1
        await reg.aclose()

    asyncio.run(driver())
