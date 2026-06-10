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
from mtui.mcp.registry import SessionRegistry, _log_label, _session_key
from mtui.mcp.session import McpSession

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


def _registry(tmp_path: Path) -> SessionRegistry:
    """A registry wired to the real :func:`build_session` factory."""
    return SessionRegistry(build_session, _config(tmp_path), _LOG)


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
    assert a._lock is not b._lock  # noqa: SLF001


def test_get_or_create_concurrent_first_calls_mint_one(tmp_path: Path) -> None:
    """A burst of concurrent first-calls for one key creates exactly one session.

    The factory is wrapped so we can count constructions; the registry
    lock + double-checked read must collapse the race to a single mint.
    """
    calls = {"n": 0}

    def counting_factory(cfg: Config, log: Logger) -> McpSession:
        calls["n"] += 1
        return build_session(cfg, log)

    reg = SessionRegistry(counting_factory, _config(tmp_path), _LOG)

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

        session.close = _close  # ty: ignore[unresolved-attribute]
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

        session.close = _close  # ty: ignore[unresolved-attribute]
        await reg.evict("k1")  # must not raise

    asyncio.run(driver())
