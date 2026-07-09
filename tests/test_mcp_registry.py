"""Tests for :mod:`mtui.mcp.registry`.

Covers the per-client isolation contract Phase B introduces:

* :func:`_session_key` returns ``str(id(ctx.session))`` and refuses a
  ``None`` context;
* :func:`_log_label` prefers the ``Mcp-Session-Id`` header, degrades to
  ``request_id`` / key, and never raises on a malformed context;
* :meth:`SessionRegistry.get_or_create` returns the *same* instance for
  one key and *independent* sessions (own ``metadata`` / ``targets`` /
  lock) for different keys;
* a burst of concurrent first-calls for one key mints exactly one
  session (double-checked locking);
* :meth:`SessionRegistry.evict` drops the key, awaits a ``close``
  coroutine when present, and is idempotent.

No ``[mcp]`` extra is required: the registry only touches
:class:`McpSession` and plain objects, so a tiny stand-in stands in for
:class:`mcp.server.fastmcp.Context`.
"""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING
from unittest.mock import MagicMock

import pytest

from mtui.mcp.main import build_session
from mtui.mcp.registry import (
    SessionRegistry,
    SessionRegistryFullError,
    _log_label,
    _session_key,
)
from mtui.mcp.session import McpSession
from mtui.types import Workflow

if TYPE_CHECKING:
    from logging import Logger

    from mtui.support.config import Config

_LOG = logging.getLogger("test.mcp.registry")


# --------------------------------------------------------------------------- #
# Fixtures / helpers                                                          #
# --------------------------------------------------------------------------- #


def _config(tmp_path: Path) -> MagicMock:
    """The minimal Config shape McpSession's constructor touches."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


class _RealishConfig:
    """Minimal plain-object config that ``copy.copy`` duplicates faithfully.

    A ``MagicMock`` cannot stand in here: ``copy.copy(MagicMock())``
    yields another mock whose ``.auto`` attribute is a truthy ``Mock``,
    masking the ``False`` seeding :func:`build_session` performs. This
    plain class behaves like the real :class:`mtui.support.config.Config`
    for the handful of attributes the session constructor and
    ``build_session`` touch, while staying cheap to instantiate.
    """

    def __init__(self, tmp_path: Path) -> None:
        self.template_dir = tmp_path
        self.target_tempdir = tmp_path / "target"
        self.chdir_to_template_dir = False
        self.connection_timeout = 30
        self.session_user = "testuser"


def _registry(
    tmp_path: Path, *, idle_timeout: float = 0.0, max_sessions: int = 32
) -> SessionRegistry:
    """A registry wired to the real :func:`build_session` factory.

    ``idle_timeout`` defaults to ``0`` (sweeper disabled) so the common
    keying/isolation tests do not spawn a background task; the
    sweeper-specific tests opt in with a small positive value.
    """
    return SessionRegistry(
        build_session,
        _config(tmp_path),
        _LOG,
        max_sessions=max_sessions,
        idle_timeout=idle_timeout,
    )


def _ctx_with_session(obj: object) -> SimpleNamespace:
    """A fake Context whose ``.session`` is ``obj`` (for keying)."""
    return SimpleNamespace(session=obj)


# --------------------------------------------------------------------------- #
# _session_key                                                                #
# --------------------------------------------------------------------------- #


def test_session_key_is_id_of_ctx_session() -> None:
    """The key is the stringified identity of ``ctx.session``."""
    sentinel = object()
    ctx = _ctx_with_session(sentinel)
    assert _session_key(ctx) == str(id(sentinel))


def test_session_key_stable_across_calls_for_same_session() -> None:
    """Two reads of the same context yield the same key."""
    ctx = _ctx_with_session(object())
    assert _session_key(ctx) == _session_key(ctx)


def test_session_key_differs_for_distinct_sessions() -> None:
    """Distinct ``ServerSession`` objects produce distinct keys."""
    assert _session_key(_ctx_with_session(object())) != _session_key(
        _ctx_with_session(object())
    )


def test_session_key_raises_on_none_ctx() -> None:
    """``None`` context must raise so callers route it to a static provider."""
    with pytest.raises(ValueError, match="without a request Context"):
        _session_key(None)


# --------------------------------------------------------------------------- #
# _log_label                                                                  #
# --------------------------------------------------------------------------- #


def test_log_label_none_ctx() -> None:
    assert _log_label(None) == "<no-ctx>"


def test_log_label_prefers_mcp_session_id_header() -> None:
    """A present ``Mcp-Session-Id`` header wins (case-insensitive get)."""
    headers = {"mcp-session-id": "abc-123"}
    request = SimpleNamespace(headers=headers)
    ctx = SimpleNamespace(request_context=SimpleNamespace(request=request))
    assert _log_label(ctx) == "abc-123"


def test_log_label_falls_back_to_request_id() -> None:
    """No header → ``req=<request_id>``."""
    ctx = SimpleNamespace(
        request_context=SimpleNamespace(request=None, request_id="rid-7")
    )
    assert _log_label(ctx) == "req=rid-7"


def test_log_label_never_raises_on_garbage_ctx() -> None:
    """A context missing every attribute degrades, never raises."""
    label = _log_label(SimpleNamespace())
    assert isinstance(label, str)
    assert label  # non-empty


# --------------------------------------------------------------------------- #
# get_or_create                                                               #
# --------------------------------------------------------------------------- #


def test_get_or_create_same_key_returns_same_instance(tmp_path: Path) -> None:
    """One key → one cached session across repeated calls."""
    reg = _registry(tmp_path)

    async def driver() -> tuple[McpSession, McpSession]:
        a = await reg.get_or_create("k1")
        b = await reg.get_or_create("k1")
        return a, b

    a, b = asyncio.run(driver())
    assert a is b
    assert isinstance(a, McpSession)


def test_get_or_create_distinct_keys_are_isolated(tmp_path: Path) -> None:
    """Different keys get independent sessions, metadata, targets and locks."""
    reg = _registry(tmp_path)

    async def driver() -> tuple[McpSession, McpSession]:
        return await reg.get_or_create("k1"), await reg.get_or_create("k2")

    a, b = asyncio.run(driver())
    assert a is not b
    assert a.metadata is not b.metadata
    assert a.targets is not b.targets
    # Each session owns its own registry gate and per-RRID lock map, so work on
    # one client never blocks another.
    assert a._registry is not b._registry  # noqa: SLF001
    assert a._rrid_locks is not b._rrid_locks  # noqa: SLF001


def test_get_or_create_concurrent_first_calls_mint_one(tmp_path: Path) -> None:
    """A burst of concurrent first-calls for one key creates exactly one session.

    The factory is wrapped so we can count constructions; the registry
    lock + double-checked read must collapse the race to a single mint.
    """
    calls = {"n": 0}

    def counting_factory(cfg: Config, log: Logger) -> McpSession:
        calls["n"] += 1
        return build_session(cfg, log)

    reg = SessionRegistry(counting_factory, _config(tmp_path), _LOG, idle_timeout=0.0)

    async def driver() -> list[McpSession]:
        return await asyncio.gather(*(reg.get_or_create("dup") for _ in range(25)))

    sessions = asyncio.run(driver())
    assert calls["n"] == 1
    assert all(s is sessions[0] for s in sessions)


# --------------------------------------------------------------------------- #
# evict                                                                       #
# --------------------------------------------------------------------------- #


def test_evict_removes_key_and_is_idempotent(tmp_path: Path) -> None:
    """Evicting drops the key; a second evict is a no-op and a refetch mints anew."""
    reg = _registry(tmp_path)

    async def driver() -> bool:
        a = await reg.get_or_create("k1")
        await reg.evict("k1")
        await reg.evict("k1")  # must not raise
        b = await reg.get_or_create("k1")
        return a is b

    # A fresh get_or_create after eviction mints a *new* session.
    assert asyncio.run(driver()) is False


def test_evict_awaits_close_when_present(tmp_path: Path) -> None:
    """When a session exposes ``close``, eviction awaits it once."""
    reg = _registry(tmp_path)
    closed = {"n": 0}

    async def driver() -> None:
        session = await reg.get_or_create("k1")

        async def _close() -> None:
            closed["n"] += 1

        session.close = _close  # ty: ignore[invalid-assignment]
        await reg.evict("k1")

    asyncio.run(driver())
    assert closed["n"] == 1


def test_evict_swallows_close_errors(tmp_path: Path) -> None:
    """A failing ``close`` during eviction must not propagate."""
    reg = _registry(tmp_path)

    async def driver() -> None:
        session = await reg.get_or_create("k1")

        async def _close() -> None:
            raise RuntimeError("boom")

        session.close = _close  # ty: ignore[invalid-assignment]
        await reg.evict("k1")  # must not raise

    asyncio.run(driver())


def test_evict_calls_real_session_close_and_removes_key(tmp_path: Path) -> None:
    """``evict`` invokes the real :meth:`McpSession.close` and drops the key.

    Spies on the minted session's ``close`` (a real coroutine on
    McpSession now) to prove eviction both tears the session down and
    removes it from the registry.
    """
    reg = _registry(tmp_path)
    closed = {"n": 0}

    async def driver() -> bool:
        session = await reg.get_or_create("k1")
        real_close = session.close

        async def _spy() -> None:
            closed["n"] += 1
            await real_close()

        session.close = _spy  # ty: ignore[invalid-assignment]
        await reg.evict("k1")
        # Key gone -> a refetch mints a brand-new session.
        again = await reg.get_or_create("k1")
        return again is session

    refetch_is_same = asyncio.run(driver())
    assert closed["n"] == 1
    assert refetch_is_same is False


# --------------------------------------------------------------------------- #
# Session cap (DoS guard)                                                     #
# --------------------------------------------------------------------------- #


def test_cap_refuses_creation_past_limit(tmp_path: Path) -> None:
    """Creating one session past ``max_sessions`` raises the documented error."""
    reg = _registry(tmp_path, max_sessions=2)

    async def driver() -> None:
        await reg.get_or_create("k1")
        await reg.get_or_create("k2")
        # Third distinct key would exceed the cap of 2.
        await reg.get_or_create("k3")

    with pytest.raises(SessionRegistryFullError, match="session registry full"):
        asyncio.run(driver())


def test_cap_does_not_count_existing_keys(tmp_path: Path) -> None:
    """Re-requesting an existing key never trips the cap."""
    reg = _registry(tmp_path, max_sessions=1)

    async def driver() -> McpSession:
        first = await reg.get_or_create("k1")
        # Same key, many times: must not raise even at cap == 1.
        for _ in range(5):
            again = await reg.get_or_create("k1")
            assert again is first
        return first

    assert isinstance(asyncio.run(driver()), McpSession)


def test_cap_frees_a_slot_after_evict(tmp_path: Path) -> None:
    """Evicting a session frees a slot so a new key can be created."""
    reg = _registry(tmp_path, max_sessions=1)

    async def driver() -> McpSession:
        await reg.get_or_create("k1")
        await reg.evict("k1")
        # Slot freed -> a different key now fits.
        return await reg.get_or_create("k2")

    assert isinstance(asyncio.run(driver()), McpSession)


# --------------------------------------------------------------------------- #
# Idle-TTL sweeper                                                            #
# --------------------------------------------------------------------------- #


def test_idle_sweeper_evicts_stale_session(tmp_path: Path) -> None:
    """A session untouched past a short TTL is swept and closed automatically.

    Uses a tiny ``idle_timeout`` so the sweeper (wake interval = ttl/2,
    floored at 1s) reaps within a couple of seconds. We spy on the
    session's ``close`` to confirm teardown fired, then assert the key
    is gone.
    """
    reg = _registry(tmp_path, idle_timeout=1.0)
    closed = {"n": 0}

    async def driver() -> int:
        session = await reg.get_or_create("k1")
        real_close = session.close

        async def _spy() -> None:
            closed["n"] += 1
            await real_close()

        session.close = _spy  # ty: ignore[invalid-assignment]

        # Wait long enough for one sweep cycle (interval == max(1, ttl/2)
        # == 1s) plus the ttl to elapse with margin.
        for _ in range(40):
            await asyncio.sleep(0.1)
            if "k1" not in reg._sessions:  # noqa: SLF001
                break
        await reg.aclose()
        return closed["n"]

    n_closed = asyncio.run(driver())
    assert n_closed == 1


def test_fresh_activity_keeps_session_alive(tmp_path: Path) -> None:
    """Touching a session within the TTL prevents it from being swept."""
    reg = _registry(tmp_path, idle_timeout=1.0)

    async def driver() -> bool:
        first = await reg.get_or_create("k1")
        # Keep touching it under the TTL for ~1.5s of wall time.
        for _ in range(15):
            await asyncio.sleep(0.1)
            again = await reg.get_or_create("k1")
            assert again is first
        alive = "k1" in reg._sessions  # noqa: SLF001
        await reg.aclose()
        return alive

    assert asyncio.run(driver()) is True


def test_sweeper_disabled_when_idle_timeout_zero(tmp_path: Path) -> None:
    """``idle_timeout <= 0`` starts no sweeper task."""
    reg = _registry(tmp_path, idle_timeout=0.0)

    async def driver() -> object:
        await reg.get_or_create("k1")
        return reg._sweeper  # noqa: SLF001

    assert asyncio.run(driver()) is None


def test_sweep_spares_session_reactivated_during_the_sweep(tmp_path: Path) -> None:
    """A session re-activated while the sweep runs must not be reaped.

    The sweep snapshots the stale keys, then evicts them one by one.
    Eviction awaits ``close()``; while one eviction is blocked on a slow
    close, ``get_or_create`` can hand a *later* stale key's session to a
    client (refreshing its last-touch). The sweep used to evict it anyway,
    closing SSH host connections out from under the active client.
    Staleness is now re-validated immediately before each eviction.
    """
    reg = _registry(tmp_path, idle_timeout=4.0)  # sweep interval = 2s
    import time as _time

    close_started = asyncio.Event()
    release_close = asyncio.Event()
    k2_closed = {"n": 0}

    async def driver() -> None:
        s1 = await reg.get_or_create("k1")
        s2 = await reg.get_or_create("k2")

        async def _slow_close() -> None:
            close_started.set()
            await release_close.wait()

        async def _k2_close() -> None:
            k2_closed["n"] += 1

        s1.close = _slow_close  # ty: ignore[invalid-assignment]
        s2.close = _k2_close  # ty: ignore[invalid-assignment]

        # Age both sessions well past the TTL so the next sweep round
        # snapshots stale = [k1, k2] (insertion order).
        aged = _time.monotonic() - 100
        reg._last_touch["k1"] = aged  # noqa: SLF001
        reg._last_touch["k2"] = aged  # noqa: SLF001

        # Wait until the sweeper is mid-eviction of k1 (blocked in close),
        # then do what a live client does: grab k2, refreshing its touch.
        await asyncio.wait_for(close_started.wait(), timeout=10)
        again = await reg.get_or_create("k2")
        assert again is s2

        # Let the k1 eviction finish; the sweep round proceeds to k2.
        release_close.set()
        await asyncio.sleep(0.5)

        assert "k1" not in reg._sessions  # noqa: SLF001 -- genuinely idle: reaped
        assert "k2" in reg._sessions  # noqa: SLF001 -- re-activated: spared
        assert k2_closed["n"] == 0
        await reg.aclose()

    asyncio.run(driver())


def test_sweep_skips_key_already_evicted_during_the_sweep(tmp_path: Path) -> None:
    """A stale-listed key evicted by someone else mid-sweep is skipped.

    While the sweep round is blocked in the first eviction's ``close()``,
    a client disconnect can evict a later stale-listed key directly. When
    the round reaches that key its last-touch is gone; the re-check must
    skip it rather than close the session a second time.
    """
    reg = _registry(tmp_path, idle_timeout=4.0)  # sweep interval = 2s
    import time as _time

    close_started = asyncio.Event()
    release_close = asyncio.Event()
    k2_closed = {"n": 0}

    async def driver() -> None:
        s1 = await reg.get_or_create("k1")
        s2 = await reg.get_or_create("k2")

        async def _slow_close() -> None:
            close_started.set()
            await release_close.wait()

        async def _k2_close() -> None:
            k2_closed["n"] += 1

        s1.close = _slow_close  # ty: ignore[invalid-assignment]
        s2.close = _k2_close  # ty: ignore[invalid-assignment]

        aged = _time.monotonic() - 100
        reg._last_touch["k1"] = aged  # noqa: SLF001
        reg._last_touch["k2"] = aged  # noqa: SLF001

        # Mid-sweep (blocked in k1's close), a client disconnect evicts k2.
        await asyncio.wait_for(close_started.wait(), timeout=10)
        await reg.evict("k2")
        assert k2_closed["n"] == 1

        # The round proceeds to k2: last-touch is gone -> skipped, close
        # is NOT called a second time.
        release_close.set()
        await asyncio.sleep(0.5)

        assert "k1" not in reg._sessions  # noqa: SLF001
        assert "k2" not in reg._sessions  # noqa: SLF001
        assert k2_closed["n"] == 1
        await reg.aclose()

    asyncio.run(driver())


def test_aclose_cancels_sweeper_and_closes_all(tmp_path: Path) -> None:
    """``aclose`` cancels the sweeper task and evicts every live session."""
    reg = _registry(tmp_path, idle_timeout=30.0)

    async def driver() -> tuple[bool, int]:
        await reg.get_or_create("k1")
        await reg.get_or_create("k2")
        sweeper_running = reg._sweeper is not None  # noqa: SLF001
        await reg.aclose()
        return sweeper_running, len(reg._sessions)

    sweeper_running, live_after = asyncio.run(driver())
    assert sweeper_running is True
    assert live_after == 0


# --------------------------------------------------------------------------- #
# build_session: default workflow mode + per-session config isolation         #
# --------------------------------------------------------------------------- #


def test_build_session_defaults_to_manual_mode(tmp_path: Path) -> None:
    """A fresh session's (null) report defaults to manual workflow.

    Workflow mode now lives on the loaded :class:`TestReport` as a
    :class:`~mtui.types.Workflow` enum, not on ``config``. A fresh
    session holds a ``NullTestReport`` whose ``workflow`` defaults to
    ``Workflow.MANUAL``, and ``build_session`` no longer seeds any mode
    flags onto ``config``.
    """
    session = build_session(_RealishConfig(tmp_path), _LOG)  # ty: ignore[invalid-argument-type]
    assert session.metadata.workflow is Workflow.MANUAL
    # The mode is no longer carried by config.
    assert not hasattr(session.config, "auto")
    assert not hasattr(session.config, "kernel")
    assert not hasattr(session.config, "workflow")


def test_build_session_copies_config_per_session(tmp_path: Path) -> None:
    """Each session gets its own config copy; mutable scalars don't leak.

    Under http every session is minted from one base ``cfg``; a shallow
    copy per session keeps mutable scalars independent so one client
    rebinding a config scalar cannot change another client's config.
    """
    base = _RealishConfig(tmp_path)
    a = build_session(base, _LOG)  # ty: ignore[invalid-argument-type]
    b = build_session(base, _LOG)  # ty: ignore[invalid-argument-type]

    assert a.config is not b.config
    # Simulate client A rebinding a config scalar.
    a.config.connection_timeout = 999
    # Client B must be unaffected.
    assert b.config.connection_timeout == 30
    # And the shared base config must never have been mutated.
    assert base.connection_timeout == 30


def test_registry_sessions_have_independent_config(tmp_path: Path) -> None:
    """Two registry-minted sessions for distinct keys carry independent configs."""
    reg = SessionRegistry(
        build_session,
        _RealishConfig(tmp_path),  # ty: ignore[invalid-argument-type]
        _LOG,
        idle_timeout=0.0,
    )

    async def driver() -> tuple[McpSession, McpSession]:
        return await reg.get_or_create("k1"), await reg.get_or_create("k2")

    a, b = asyncio.run(driver())
    assert a.config is not b.config
    a.config.connection_timeout = 999
    assert b.config.connection_timeout == 30


def test_registry_sessions_have_distinct_arbiter_owner_keys(tmp_path: Path) -> None:
    """Same RRID in two http sessions yields distinct ``(registry.id, RRID)`` owners.

    Each minted :class:`McpSession` owns its own :class:`TemplateRegistry` with
    a stable, unique ``id``. The host arbiter keys ownership on
    ``(registry.id, RRID)``, so two http clients loading the *same* RRID are
    distinct arbiter owners and never block each other beyond real host
    contention. This locks Phase 4 step 4 in.
    """
    reg = SessionRegistry(
        build_session,
        _RealishConfig(tmp_path),  # ty: ignore[invalid-argument-type]
        _LOG,
        idle_timeout=0.0,
    )

    async def driver() -> tuple[McpSession, McpSession]:
        return await reg.get_or_create("k1"), await reg.get_or_create("k2")

    a, b = asyncio.run(driver())
    # Distinct registries with distinct stable ids.
    assert a.templates is not b.templates
    assert a.templates.id != b.templates.id

    rrid = "SUSE:Maintenance:1:1"
    owner_a = (a.templates.id, rrid)
    owner_b = (b.templates.id, rrid)
    assert owner_a != owner_b
