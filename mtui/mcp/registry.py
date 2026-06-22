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

Lifecycle is registry-owned because the MCP SDK gives no per-session
teardown callback we can reach through :meth:`FastMCP.run` (its
``streamable-http`` path exposes no ``session_idle_timeout``, and a
``ServerSession`` has no close hook): a background idle-TTL sweeper
evicts sessions that have gone quiet for ``idle_timeout`` seconds and
:meth:`McpSession.close`-es their hosts, and a ``max_sessions`` cap
refuses new-session creation past the bound (DoS guard) rather than
spawning unbounded ``targets`` / threads.
"""

from __future__ import annotations

import asyncio
import contextlib
import time
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

#: The workspace name a tool call uses when the caller does not pass one.
#: A single ``default`` workspace reproduces the pre-workspace behaviour
#: (one loaded template per client), so every existing call keeps working.
WORKSPACE_DEFAULT: str = "default"

#: Separator joining the per-client base key and the workspace name into
#: the registry key. ``\x1f`` (ASCII unit separator) cannot appear in the
#: numeric ``id()`` base nor, in practice, in a workspace name, so the
#: composite key round-trips unambiguously (see :func:`split_workspace_key`).
_WORKSPACE_SEP: str = "\x1f"


def workspace_key(base: str, workspace: str) -> str:
    """Compose the registry key for ``workspace`` under client ``base``.

    The base is the per-client key (``id(ctx.session)`` under http, or
    :data:`DEFAULT_SESSION_KEY` for context-less calls); appending the
    workspace name lets one client hold several independent sessions —
    each its own loaded template + ``targets`` — addressed by name.
    """
    return f"{base}{_WORKSPACE_SEP}{workspace}"


def split_workspace_key(key: str) -> tuple[str, str]:
    """Inverse of :func:`workspace_key`: ``key`` -> ``(base, workspace)``.

    A key without the separator (legacy/non-workspace) is returned as
    ``(key, WORKSPACE_DEFAULT)`` so callers can treat it uniformly.
    """
    base, sep, ws = key.partition(_WORKSPACE_SEP)
    return (base, ws) if sep else (key, WORKSPACE_DEFAULT)


#: Default ceiling on concurrent client sessions (DoS guard). Overridden
#: from ``[mcp] session_cap`` via :func:`mtui.mcp.main.main`.
DEFAULT_MAX_SESSIONS: int = 32

#: Default idle-TTL, in seconds, after which a quiet session is swept.
#: Overridden from ``[mcp] session_idle_timeout``. ``0`` disables the
#: sweeper.
DEFAULT_IDLE_TIMEOUT_SECONDS: float = 1800.0


class SessionRegistryFullError(RuntimeError):
    """Raised when creating a new session would exceed ``max_sessions``.

    Surfaced to the client as the failing tool call's error so a
    misbehaving or runaway fleet of clients gets a clear, bounded
    refusal instead of silently exhausting the server's SSH/thread
    budget.
    """


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


async def resolve_session(
    provider: SessionProvider,
    ctx: Any | None,
    workspace: str = WORKSPACE_DEFAULT,
) -> McpSession:
    """Resolve the per-call session from ``provider`` for request ``ctx``.

    Routes the ``ctx is None`` case (direct-call tests, non-request
    invocation) to :data:`DEFAULT_SESSION_KEY` instead of computing a
    key off a missing context, so the existing direct-call tests keep
    working unchanged. Otherwise keys on :func:`_session_key`.

    ``workspace`` lets a single client hold several independent sessions
    (each its own loaded template + ``targets``): the per-client base key
    is combined with the workspace name via :func:`workspace_key`. The
    default ``"default"`` reproduces the one-session-per-client behaviour,
    so callers that do not pass a workspace are unaffected. Under the
    static single-entry provider (:meth:`McpSession.get_or_create`) the
    composite key is ignored, so stdio without a workspace registry still
    returns the one session.

    Args:
        provider: The session provider (registry or static session).
        ctx: The FastMCP :class:`~mcp.server.fastmcp.Context`, or
            ``None``.
        workspace: The named workspace within this client (default
            :data:`WORKSPACE_DEFAULT`).

    Returns:
        The :class:`McpSession` to dispatch this call through.

    """
    base = DEFAULT_SESSION_KEY if ctx is None else _session_key(ctx)
    return await provider.get_or_create(workspace_key(base, workspace))


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

    Two safety bounds:

    * ``max_sessions`` caps how many sessions may coexist; creating one
      past the cap raises :class:`SessionRegistryFullError` (DoS guard).
    * ``idle_timeout`` drives a lazily-started background sweeper that
      evicts (and :meth:`McpSession.close`-es) any session untouched
      for that many seconds. A non-positive timeout disables sweeping.
    """

    def __init__(
        self,
        factory: Callable[[Config, Logger], McpSession],
        cfg: Config,
        log: Logger,
        *,
        max_sessions: int = DEFAULT_MAX_SESSIONS,
        idle_timeout: float = DEFAULT_IDLE_TIMEOUT_SECONDS,
    ) -> None:
        """Store the session factory, base ``cfg`` / ``log``, and bounds.

        Args:
            factory: Callable that mints a fresh session from
                ``(cfg, log)`` — :func:`mtui.mcp.main.build_session`.
            cfg: The base configuration handed to every minted session.
            log: The server logger handed to every minted session.
            max_sessions: Ceiling on concurrent sessions; exceeding it
                raises :class:`SessionRegistryFullError`.
            idle_timeout: Seconds of inactivity before a session is
                swept; ``<= 0`` disables the sweeper.

        """
        self._factory = factory
        self._cfg = cfg
        self._log = log
        self._max_sessions = max_sessions
        self._idle_timeout = idle_timeout
        self._sessions: dict[str, McpSession] = {}
        self._last_touch: dict[str, float] = {}
        self._lock = asyncio.Lock()
        self._sweeper: asyncio.Task[None] | None = None

    async def get_or_create(self, key: str) -> McpSession:
        """Return the session for ``key``, minting one on first use.

        Double-checked locking: the common hot path (key already
        present) reads the dict without contending on the registry
        lock; only a miss takes the lock, re-checks, and creates. So a
        flurry of concurrent first-calls for one key produces exactly
        one session. Every call refreshes the key's last-touch
        timestamp so the idle sweeper only reaps genuinely quiet
        sessions, and the first call lazily starts the sweeper (we are
        guaranteed to be inside the event loop here).

        Args:
            key: The per-client session key from :func:`_session_key`.

        Returns:
            The :class:`McpSession` bound to ``key``.

        Raises:
            SessionRegistryFullError: If a new session is required but the
                registry already holds ``max_sessions`` sessions.

        """
        self._ensure_sweeper()
        session = self._sessions.get(key)
        if session is not None:
            self._last_touch[key] = time.monotonic()
            return session
        async with self._lock:
            session = self._sessions.get(key)
            if session is None:
                if len(self._sessions) >= self._max_sessions:
                    raise SessionRegistryFullError(
                        f"session registry full: {self._max_sessions} concurrent "
                        "client sessions already active; retry once a session is "
                        "released or raise [mcp] session_cap"
                    )
                session = self._factory(self._cfg, self._log)
                self._sessions[key] = session
                logger.info(
                    "minted new MCP session (key=%s, live=%d)",
                    key,
                    len(self._sessions),
                )
            self._last_touch[key] = time.monotonic()
            return session

    def live_sessions(self) -> dict[str, McpSession]:
        """Return a snapshot ``{key: session}`` of the live sessions.

        A shallow copy so the caller (the ``list_workspaces`` tool) can
        iterate without racing concurrent :meth:`get_or_create` /
        :meth:`evict` mutations of the underlying dict.
        """
        return dict(self._sessions)

    async def evict(self, key: str) -> None:
        """Drop ``key`` from the registry and best-effort close it.

        Pops the session (no-op if absent), drops its last-touch
        bookkeeping, and awaits :meth:`McpSession.close` under
        suppression so a teardown error never propagates. Safe to call
        more than once for the same key.

        Args:
            key: The per-client session key to evict.

        """
        session = self._sessions.pop(key, None)
        self._last_touch.pop(key, None)
        if session is None:
            return
        close = getattr(session, "close", None)
        if close is not None:
            try:
                await close()
            except Exception as exc:  # noqa: BLE001 - teardown is best-effort
                logger.warning("error closing session %s: %s", key, exc)
        logger.info("evicted MCP session (key=%s, live=%d)", key, len(self._sessions))

    def _ensure_sweeper(self) -> None:
        """Lazily start the idle-TTL sweeper task on first use.

        Started from :meth:`get_or_create` (always inside the running
        loop) rather than ``__init__`` (which runs before
        :meth:`FastMCP.run` enters the loop). A non-positive
        ``idle_timeout`` disables sweeping entirely.
        """
        if self._idle_timeout <= 0 or self._sweeper is not None:
            return
        self._sweeper = asyncio.create_task(self._sweep_loop())

    async def _sweep_loop(self) -> None:
        """Periodically evict sessions idle past ``idle_timeout``.

        Wakes every ``idle_timeout / 2`` seconds (bounded to a sane
        floor), collects keys whose last-touch is older than the
        timeout, and evicts each outside the registry lock (so a slow
        :meth:`McpSession.close` cannot stall fresh
        :meth:`get_or_create` calls). Runs until cancelled by
        :meth:`aclose`.
        """
        interval = max(1.0, self._idle_timeout / 2)
        try:
            while True:
                await asyncio.sleep(interval)
                now = time.monotonic()
                stale = [
                    key
                    for key, touched in self._last_touch.items()
                    if now - touched >= self._idle_timeout
                ]
                for key in stale:
                    logger.info("sweeping idle MCP session (key=%s)", key)
                    await self.evict(key)
        except asyncio.CancelledError:
            raise

    async def aclose(self) -> None:
        """Cancel the sweeper and close every live session.

        Used for graceful shutdown and by tests to avoid leaking a
        background task. Idempotent.
        """
        if self._sweeper is not None:
            self._sweeper.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._sweeper
            self._sweeper = None
        for key in list(self._sessions):
            await self.evict(key)
