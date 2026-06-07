"""The main entry point for the ``mtui-mcp`` MCP server.

Parses CLI args, builds the same :class:`Config` the REPL does, runs
:func:`detect_system`, lazily imports :mod:`mcp.server.fastmcp` so a
missing ``[mcp]`` extra produces a friendly hint instead of a
traceback, then constructs an :class:`McpSession`, optionally preloads
an update and autoconnects SUTs, registers every non-denied command
plus the three testreport tools, and dispatches on the chosen
transport.

Preload and autoconnect both **log-and-continue** on failure: an MCP
session is long-lived, the LLM has its own recovery paths
(``load_template``, ``add_host``), and refusing to boot would force a
manual restart for a single typo. The REPL's stricter "exit 1 on
preload failure" contract does not translate cleanly to a server.
"""

import asyncio
import logging
import shlex
import sys
from subprocess import CalledProcessError

from ..cli.argparse import ArgsParseFailureError
from ..cli.colors import create_logger
from ..cli.colors import set_mode as set_color_mode
from ..commands import Command
from ..support.config import Config
from ..support.exceptions import MissingGiteaTokenError
from ..support.messages import MetadataNotLoadedError, SvnCheckoutInterruptedError
from ..support.systemcheck import detect_system
from .args import get_parser
from .session import McpCommandError, McpSession
from .testreport_tools import register_testreport_tools
from .tools import build_tools

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
    in :class:`BaseExceptionGroup` on Python 3.11+, so a bare
    ``except KeyboardInterrupt`` does not catch Ctrl-C delivered to an
    active task group. We walk the group recursively and only treat it
    as a clean shutdown when *every* leaf is one of
    :data:`_SHUTDOWN_LEAVES`; if any leaf is a real error we let the
    group propagate so the crash path still triggers.
    """
    if isinstance(exc, BaseExceptionGroup):
        return all(_is_clean_shutdown_group(e) for e in exc.exceptions)
    return isinstance(exc, _SHUTDOWN_LEAVES)


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
    cfg.kernel = False
    cfg.auto = False
    cfg.distro, cfg.distro_ver, cfg.distro_kernel = detect_system()

    session = McpSession(cfg, logger)

    # Preload an explicitly requested update. Log-and-continue: a
    # failure here leaves `session.metadata` as the NullTestReport the
    # constructor already installed, so testreport_* tools refuse with
    # their well-defined "no testreport loaded" message instead of
    # tearing the server down.
    if args.update:
        if args.update.kind == "kernel":
            cfg.kernel = True
            cfg.auto = False
        elif args.update.kind == "auto":
            cfg.auto = True
            cfg.kernel = False
        try:
            session.load_update(args.update, autoconnect=not bool(args.sut))
        except KeyboardInterrupt:
            logger.info("mtui-mcp: shutting down")
            return 0
        except (
            SvnCheckoutInterruptedError,
            CalledProcessError,
            MetadataNotLoadedError,
            MissingGiteaTokenError,
        ) as e:
            logger.warning(
                "failed to preload update %s: %s; continuing without a "
                "loaded testreport",
                args.update,
                e,
            )

    # Autoconnect SUTs by dispatching through the registered `add_host`
    # command, exactly like the REPL does. We are pre-server here so
    # the asyncio loop has not started yet; calling the synchronous
    # core directly avoids spinning up and tearing down a loop just to
    # invoke code that the lock would not even contend on (single
    # thread, no concurrent caller exists yet).
    if args.sut:
        add_host_cls = Command.registry.get("add_host")
        if add_host_cls is None:
            logger.error("add_host command missing from registry; cannot autoconnect")
        else:
            try:
                for x in args.sut:
                    try:
                        session._run_sync(add_host_cls, shlex.split(x.print_args()))  # noqa: SLF001
                    except McpCommandError as e:
                        logger.error("failed to add host %s: %s", x, e)
            except KeyboardInterrupt:
                logger.info("mtui-mcp: shutting down")
                return 0

    # ``host``/``port`` are constructor-time settings in the SDK and
    # only consulted under the ``streamable-http`` transport; passing
    # them under stdio is a harmless no-op.
    mcp = FastMCP(name="mtui", host=args.host, port=args.port)
    build_tools(mcp, session)
    register_testreport_tools(mcp, session)

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
