"""Per-client :class:`McpSession` registry for the http transport.

Under ``--transport http`` a single ``mtui-mcp`` process serves many
concurrent MCP clients. Each client must see **only its own** loaded
template and SSH ``targets`` — sharing one global session would let one
client's ``load_template`` clobber another's. The MCP SDK keys its own
connection bookkeeping by ``Mcp-Session-Id`` internally but exposes no
per-session application-state hook, so this registry is
application-owned.

:class:`SessionRegistry` maps a per-client key to a lazily-minted,
fully isolated :class:`McpSession` (own ``metadata`` / ``targets`` /
lock). The key is :func:`id` of the request's ``ServerSession`` object
(``ctx.session``), which is 1:1 with the MCP session and present in
every tool call; the ``Mcp-Session-Id`` header is read for log lines
only (never load-bearing — see :func:`_log_label`).

The registry exposes the same ``async get_or_create(key) -> McpSession``
shape as :meth:`McpSession.get_or_create` (the degenerate single-entry
provider used by stdio), so :mod:`mtui.mcp.tools` and
:mod:`mtui.mcp.testreport_tools` resolve a session per call without
caring which transport they run under.

Lifecycle (idle-TTL eviction, session cap) and :meth:`McpSession.close`
land in Phase C; :meth:`evict` is already best-effort
``close()``-aware so wiring it in is additive.
"""

from __future__ import annotations

import asyncio
from logging import getLogger
from typing import TYPE_CHECKING, Any, Protocol

from .session import McpSession

if TYPE_CHECKING:
    from collections.abc import Callable
    from logging import Logger

    from ..support.config import Config

logger = getLogger("mtui.mcp.registry")

#: Registry key used when a tool is invoked without a request Context
#: (direct-call tests, non-request paths). It collides harmlessly under
#: the static single-entry provider (:meth:`McpSession.get_or_create`
#: ignores the key) and yields one shared fallback session under the
#: http :class:`SessionRegistry`.
DEFAULT_SESSION_KEY: str = "<default>"


class SessionProvider(Protocol):
    """The minimal surface :mod:`mtui.mcp.tools` resolves a session through.

    Implemented by both :class:`SessionRegistry` (http: lazy
    per-client sessions) and :class:`mtui.mcp.session.McpSession` (the
    degenerate single-entry provider for stdio + direct tests), so the
    tool layer is transport-agnostic.
    """

    async def get_or_create(self, key: str) -> McpSession:
        """Return the session bound to ``key`` (minting one if needed)."""
        ...


async def resolve_session(provider: SessionProvider, ctx: Any | None) -> McpSession:
    """Resolve the per-call session from ``provider`` for request ``ctx``.

    Routes the ``ctx is None`` case (direct-call tests, non-request
    invocation) to :data:`DEFAULT_SESSION_KEY` instead of computing a
    key off a missing context, so the existing direct-call tests keep
    working unchanged. Otherwise keys on :func:`_session_key`.

    Args:
        provider: The session provider (registry or static session).
        ctx: The FastMCP :class:`~mcp.server.fastmcp.Context`, or
            ``None``.

    Returns:
        The :class:`McpSession` to dispatch this call through.

    """
    key = DEFAULT_SESSION_KEY if ctx is None else _session_key(ctx)
    return await provider.get_or_create(key)


def _session_key(ctx: Any | None) -> str:
    """Return the registry key for the request carried by ``ctx``.

    The key is the identity of the request's ``ServerSession`` object
    (``ctx.session`` → ``ctx.request_context.session``), which the SDK
    keeps 1:1 with the MCP session for the connection's whole lifetime
    and supplies on every tool call. Stringified so the registry dict
    key type is uniform and log-friendly.

    Args:
        ctx: The FastMCP :class:`~mcp.server.fastmcp.Context` for the
            in-flight tool call, or ``None``.

    Returns:
        ``str(id(ctx.session))``.

    Raises:
        ValueError: If ``ctx`` is ``None`` — callers must route the
            ``ctx is None`` case (direct tests, non-request invocation)
            to a static provider *before* reaching here.

    """
    if ctx is None:
        raise ValueError("cannot derive a session key without a request Context")
    return str(id(ctx.session))


def _log_label(ctx: Any | None) -> str:
    """Best-effort human-readable session label for log lines only.

    Tries the ``Mcp-Session-Id`` request header (present under the
    streamable-http transport), falling back to the SDK ``request_id``
    and finally the :func:`_session_key`. Every lookup is wrapped so a
    missing/renamed attribute or a header-less stdio request can never
    raise — this string is purely cosmetic and must never gate
    dispatch.

    Args:
        ctx: The FastMCP :class:`~mcp.server.fastmcp.Context`, or
            ``None``.

    Returns:
        A short label such as the ``Mcp-Session-Id`` value, ``req=<id>``,
        or ``id=<key>``; ``"<no-ctx>"`` when ``ctx`` is ``None``.

    """
    if ctx is None:
        return "<no-ctx>"
    try:
        headers = ctx.request_context.request.headers  # type: ignore[union-attr]
        sid = headers.get("mcp-session-id")
        if sid:
            return str(sid)
    except Exception:  # noqa: BLE001 - cosmetic only, never load-bearing
        pass
    try:
        return f"req={ctx.request_context.request_id}"
    except Exception:  # noqa: BLE001 - cosmetic only
        pass
    try:
        return f"id={_session_key(ctx)}"
    except Exception:  # noqa: BLE001 - cosmetic only
        return "<unknown>"


class SessionRegistry:
    """Maps a per-client key to an isolated :class:`McpSession`.

    Each distinct key gets its own session minted through ``factory``,
    so concurrent http clients never share ``metadata`` / ``targets`` /
    the per-session lock. Creation is guarded by a registry-wide
    :class:`asyncio.Lock` with a double-checked read so a burst of
    concurrent first-calls for the same key mints exactly one session.
    """

    def __init__(
        self,
        factory: Callable[[Config, Logger], McpSession],
        cfg: Config,
        log: Logger,
    ) -> None:
        """Store the session factory and the base ``cfg`` / ``log``.

        Args:
            factory: Callable that mints a fresh session from
                ``(cfg, log)`` — :func:`mtui.mcp.main.build_session`.
            cfg: The base configuration handed to every minted session.
            log: The server logger handed to every minted session.

        """
        self._factory = factory
        self._cfg = cfg
        self._log = log
        self._sessions: dict[str, McpSession] = {}
        self._lock = asyncio.Lock()

    async def get_or_create(self, key: str) -> McpSession:
        """Return the session for ``key``, minting one on first use.

        Double-checked locking: the common hot path (key already
        present) reads the dict without contending on the registry
        lock; only a miss takes the lock, re-checks, and creates. So a
        flurry of concurrent first-calls for one key produces exactly
        one session.

        Args:
            key: The per-client session key from :func:`_session_key`.

        Returns:
            The :class:`McpSession` bound to ``key``.

        """
        session = self._sessions.get(key)
        if session is not None:
            return session
        async with self._lock:
            session = self._sessions.get(key)
            if session is None:
                session = self._factory(self._cfg, self._log)
                self._sessions[key] = session
                logger.info(
                    "minted new MCP session (key=%s, live=%d)",
                    key,
                    len(self._sessions),
                )
            return session

    async def evict(self, key: str) -> None:
        """Drop ``key`` from the registry and best-effort close it.

        Pops the session (no-op if absent) and, when it carries a
        ``close`` coroutine (added in Phase C), awaits it under
        suppression so a teardown error never propagates. Safe to call
        more than once for the same key.

        Args:
            key: The per-client session key to evict.

        """
        session = self._sessions.pop(key, None)
        if session is None:
            return
        close = getattr(session, "close", None)
        if close is not None:
            try:
                await close()
            except Exception as exc:  # noqa: BLE001 - teardown is best-effort
                logger.warning("error closing session %s: %s", key, exc)
        logger.info("evicted MCP session (key=%s, live=%d)", key, len(self._sessions))
