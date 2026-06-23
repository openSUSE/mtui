"""Defines the command-line arguments for the ``mtui-mcp`` server."""

from pathlib import Path

from ..cli.argparse import ArgumentParser
from ..cli.args import _VerboseVersionAction


def get_parser(sys) -> ArgumentParser:
    """Creates and configures the argument parser for ``mtui-mcp``.

    The parser intentionally mirrors :func:`mtui.cli.args.get_parser` for
    the flags that ``Config.merge_args`` and the testreport-loading code
    care about (``-l``, ``-t``, ``-c``, ``-g``, ``--color``, ``--debug``,
    ``-V``) so the same ``Namespace`` shape can be reused. The REPL-only
    update/SUT flags are omitted, and three MCP-server flags
    (``--transport``, ``--host``, ``--port``) are added.

    Templates and hosts are loaded per session at runtime via the
    ``load_template`` and ``add_host`` tools, so the server takes no
    boot-time update/SUT flags.

    Args:
        sys: The ``sys`` module, used for stdout/stderr.

    Returns:
        A configured ``ArgumentParser`` instance.

    """
    parser = ArgumentParser(sys_=sys, prog="mtui-mcp")
    parser.add_argument(
        "-l", "--location", type=str, help="override config mtui.location"
    )
    parser.add_argument(
        "-t", "--template_dir", type=Path, help="override config mtui.template_dir"
    )
    parser.add_argument(
        "-w",
        "--connection_timeout",
        type=int,
        help="override config mtui.connection_timeout",
    )
    parser.add_argument(
        "-d",
        "--debug",
        action="store_true",
        default=False,
        help="enable debugging output",
    )
    parser.add_argument(
        "-V",
        "--version",
        action=_VerboseVersionAction,
        help="print mtui, Python, paramiko and openqa-client versions, then exit",
    )
    parser.add_argument(
        "-c", "--config", type=Path, default=None, help="Override default config path"
    )
    parser.add_argument(
        "--color",
        choices=["auto", "always", "never"],
        default="auto",
        help="control coloured output: auto (default; on iff stderr is a "
        "TTY and NO_COLOR is unset), always, or never",
    )
    parser.add_argument("-g", "--gitea_token", type=str, help="Gitea Access Token")
    parser.add_argument(
        "--transport",
        choices=["stdio", "http"],
        default="stdio",
        help="MCP transport to serve on (default: stdio)",
    )
    parser.add_argument(
        "--host",
        type=str,
        default="127.0.0.1",
        help="bind address for --transport http (default: 127.0.0.1)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=8000,
        help="bind port for --transport http (default: 8000)",
    )

    return parser
