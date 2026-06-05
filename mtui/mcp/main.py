"""The main entry point for the ``mtui-mcp`` MCP server.

This module is a skeleton: it parses arguments, builds ``Config``, runs
``detect_system``, and lazily imports :mod:`fastmcp` so a missing
``[mcp]`` extra produces a friendly hint instead of a traceback. The
actual FastMCP boot, tool synthesis, and transport dispatch land in
follow-up commits (see ``PLAN.md`` steps 5-8).
"""

import logging
import sys

from ..cli.argparse import ArgsParseFailureError
from ..cli.colors import create_logger
from ..cli.colors import set_mode as set_color_mode
from ..support.config import Config
from ..support.systemcheck import detect_system
from .args import get_parser


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

    try:
        import fastmcp  # noqa: F401
    except ImportError:
        logger.error(
            "fastmcp is not installed; run `zypper in python3-fastmcp`"
            " or `uv sync --extra mcp` or `pip install 'mtui[mcp]'`."
        )
        return 2

    cfg = Config(args.config)
    cfg.merge_args(args)
    cfg.kernel = False
    cfg.auto = False
    cfg.distro, cfg.distro_ver, cfg.distro_kernel = detect_system()

    logger.info(
        "mtui-mcp server skeleton; tool synthesis lands in a follow-up commit "
        "(transport=%s host=%s port=%s)",
        args.transport,
        args.host,
        args.port,
    )
    return 0
