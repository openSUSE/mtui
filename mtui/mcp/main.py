"""The main entry point for the ``mtui-mcp`` MCP server.

Parses CLI args, builds the same :class:`Config` the REPL does, runs
:func:`detect_system`, lazily imports :mod:`mcp.server.fastmcp` so a
missing ``[mcp]`` extra produces a friendly hint instead of a
traceback, then selects a session provider, registers every non-denied
command plus the three testreport tools, and dispatches on the chosen
transport.

The provider choice is the per-client isolation contract: under
``--transport http`` a :class:`mtui.mcp.registry.SessionRegistry` mints
one fully isolated :class:`McpSession` per client (keyed on the request
session), so concurrent clients never share ``metadata`` / ``targets``;
under stdio (one process == one session) a single eagerly-built
:class:`McpSession` doubles as the degenerate single-entry provider.

Templates and hosts are loaded per session at runtime via the
``load_template`` and ``add_host`` tools; the server performs no
boot-time preload or SUT autoconnect.
"""

from __future__ import annotations

import asyncio
import copy
import logging
import sys
from logging import Logger
from typing import TYPE_CHECKING

from ..cli.argparse import ArgsParseFailureError
from ..cli.colors import create_logger
from ..cli.colors import set_mode as set_color_mode
from ..support.config import Config
from ..support.http import disable_insecure_warnings, resolve_verify
from ..support.systemcheck import detect_system
from .args import get_parser
from .registry import SessionRegistry
from .session import McpSession
from .testreport_tools import register_testreport_tools
from .tools import build_tools, register_job_tools, register_workspace_tools

if TYPE_CHECKING:
    from .registry import SessionProvider


# Exception types that we treat as "user asked us to stop" rather than
# as crashes. ``CancelledError`` shows up because the MCP server runs
# under ``anyio.run``; when SIGINT cancels in-flight tasks anyio
# surfaces cancellations alongside the KeyboardInterrupt inside a
# group.
_SHUTDOWN_LEAVES: tuple[type[BaseException], ...] = (
    KeyboardInterrupt,
    SystemExit,
    asyncio.CancelledError,
)


def _is_clean_shutdown_group(exc: BaseException) -> bool:
    """Return True iff every leaf in ``exc`` is a shutdown sentinel.

    ``anyio.run`` (used by :meth:`mcp.server.fastmcp.FastMCP.run`) wraps task-group failures
    in :class:`BaseExceptionGroup`, so a bare
    ``except KeyboardInterrupt`` does not catch Ctrl-C delivered to an
    active task group. We walk the group recursively and only treat it
    as a clean shutdown when *every* leaf is one of
    :data:`_SHUTDOWN_LEAVES`; if any leaf is a real error we let the
    group propagate so the crash path still triggers.
    """
    if isinstance(exc, BaseExceptionGroup):
        return all(_is_clean_shutdown_group(e) for e in exc.exceptions)
    return isinstance(exc, _SHUTDOWN_LEAVES)


def build_session(cfg: Config, log: Logger) -> McpSession:
    """Construct a fresh :class:`McpSession` from ``cfg`` and ``log``.

    Each session gets its **own** shallow copy of ``cfg`` so the
    per-client isolation extends to the mutable scalars that commands
    flip in place — notably ``config.location`` (``set_location``). A
    shallow copy is the right tool: those attributes are scalars, so
    each session rebinds its own while the heavy read-only members (the
    parsed ``ConfigParser``, the refhosts factory) stay shared. Without
    the copy, every http client would share one ``Config`` and clobber
    each other's mutable state.

    Workflow mode (``auto`` / ``kernel``) now lives on the loaded
    :class:`TestReport`, not on ``config``, so no per-session seeding of
    those flags is needed here.

    Args:
        cfg: The base application configuration (already merged with
            CLI args and populated with ``detect_system`` results by
            :func:`main`); copied, never mutated.
        log: The configured server logger, reused by commands that
            touch ``self.prompt.log``.

    Returns:
        A new :class:`McpSession` with its own ``config`` copy,
        ``metadata`` / ``targets`` / lock.

    """
    session_cfg = copy.copy(cfg)
    return McpSession(session_cfg, log)


def main() -> int:
    """The main entry point for the ``mtui-mcp`` console script.

    Returns:
        The exit code of the application.

    """
    logger = create_logger("mtui-mcp")

    parser = get_parser(sys)
    try:
        args = parser.parse_args(sys.argv[1:])
    except ArgsParseFailureError as e:
        return e.status

    set_color_mode(args.color)

    if args.debug:
        logger.setLevel(level=logging.DEBUG)
        # Also raise the SDK's logger so protocol-level frames become
        # visible; without this `--debug` only surfaces mtui internals.
        logging.getLogger("mcp.server.fastmcp").setLevel(logging.DEBUG)

    try:
        from mcp.server.fastmcp import FastMCP
    except ImportError:
        logger.error(
            "mcp is not installed; run `zypper in python3-mcp`"
            " or `uv sync --extra mcp` or `pip install 'mtui[mcp]'`."
        )
        return 2

    cfg = Config(args.config)
    cfg.merge_args(args)
    cfg.distro, cfg.distro_ver, cfg.distro_kernel = detect_system()

    # Suppress urllib3's InsecureRequestWarning here, at boot, when the user
    # has disabled TLS verification — *before* FastMCP starts serving. The
    # MCP SDK wraps every request handler in
    # ``warnings.catch_warnings(record=True)``, which snapshots and restores
    # ``warnings.filters`` per request and re-emits any recorded warning as
    # ``logger.info("Warning: ...")``. A filter installed lazily on the first
    # request (as ``disable_insecure_warnings`` does in the REPL path) is
    # discarded when that request's ``catch_warnings`` block exits, and the
    # helper's module-level idempotency guard then blocks re-installation, so
    # the warning re-fires on every subsequent openQA request. Installing it
    # before the first request means every per-request snapshot includes it,
    # so it survives. Only when verification is actually off, mirroring the
    # per-call-site contract.
    if not resolve_verify(True, cfg.ssl_verify):
        disable_insecure_warnings()

    # Provider selection is the whole of the per-client isolation
    # contract: under http each client gets its own isolated
    # ``McpSession`` minted lazily by the registry (keyed on the
    # request session); under stdio one process == one session, so a
    # single eagerly-built session doubles as the degenerate
    # single-entry provider (its ``get_or_create`` returns ``self``).
    # ``build_tools`` / ``register_testreport_tools`` only see the
    # ``get_or_create`` shape, so they stay transport-agnostic.
    if args.transport == "http":
        provider: SessionProvider = SessionRegistry(
            build_session,
            cfg,
            logger,
            max_sessions=cfg.mcp_session_cap,
            idle_timeout=cfg.mcp_session_idle_timeout,
        )
        logger.info(
            "mtui-mcp: http transport — per-client session isolation "
            "(cap=%d, idle_timeout=%ss)",
            cfg.mcp_session_cap,
            cfg.mcp_session_idle_timeout,
        )
    else:
        # stdio is one process == one client, but a registry still earns
        # its keep: it lets that single client hold several **named
        # workspaces** (``load_template`` per workspace), each its own
        # isolated ``McpSession`` with its own loaded template + targets +
        # lock, so independent updates can be driven concurrently from one
        # connection. The idle sweeper is disabled (``idle_timeout=0``):
        # under stdio a workspace left quiet while the operator works
        # another must keep its host connections, not be reaped from under
        # them. The default workspace is minted lazily on first call, so a
        # caller that never names a workspace sees the prior single-session
        # behaviour unchanged.
        provider = SessionRegistry(
            build_session,
            cfg,
            logger,
            max_sessions=cfg.mcp_session_cap,
            idle_timeout=0,
        )
        logger.info(
            "mtui-mcp: stdio transport — named-workspace multiplexing "
            "(cap=%d, idle sweeper disabled)",
            cfg.mcp_session_cap,
        )

    # ``host``/``port`` are constructor-time settings in the SDK and
    # only consulted under the ``streamable-http`` transport; passing
    # them under stdio is a harmless no-op.
    mcp = FastMCP(name="mtui", host=args.host, port=args.port)
    build_tools(mcp, provider)
    register_testreport_tools(mcp, provider)
    register_workspace_tools(mcp, provider)
    register_job_tools(mcp, provider)

    try:
        if args.transport == "http":
            # ``--transport http`` is the user-facing flag we preserve
            # from the standalone fastmcp era; the SDK names this
            # transport ``streamable-http``.
            mcp.run(transport="streamable-http")
        else:
            mcp.run()
    except KeyboardInterrupt:
        logger.info("mtui-mcp: shutting down")
        return 0
    except BaseExceptionGroup as eg:
        # ``anyio.run`` may wrap a Ctrl-C delivered to an active task
        # group inside a BaseExceptionGroup; treat groups whose every
        # leaf is a shutdown sentinel as a clean exit, otherwise let
        # the group propagate to the crash path below.
        if _is_clean_shutdown_group(eg):
            logger.info("mtui-mcp: shutting down")
            return 0
        logger.error("mtui-mcp crashed: %s", eg)
        return 1
    except Exception as e:  # noqa: BLE001
        logger.error("mtui-mcp crashed: %s", e)
        return 1

    return 0
