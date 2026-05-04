"""Defines the command-line arguments for the mtui tool."""

import argparse
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path

from mtui import __version__

from .argparse import ArgumentParser
from .types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from .utils import SUTParse


def _dep_version(name: str) -> str:
    """Return the installed version of ``name`` or ``"unknown"``."""
    try:
        return version(name)
    except PackageNotFoundError:
        return "unknown"


class _VerboseVersionAction(argparse.Action):
    """``--version`` action printing mtui + key dependency versions."""

    def __init__(  # noqa: D401, ANN001
        self,
        option_strings,
        dest=argparse.SUPPRESS,
        default=argparse.SUPPRESS,
        help=None,  # noqa: A002
    ):
        super().__init__(
            option_strings=option_strings,
            dest=dest,
            default=default,
            nargs=0,
            help=help,
        )

    def __call__(self, parser, namespace, values, option_string=None):  # noqa: ANN001, D401
        import sys as _sys

        py = ".".join(str(p) for p in _sys.version_info[:3])
        lines = [
            f"mtui {__version__}",
            f"Python {py}",
            f"paramiko {_dep_version('paramiko')}",
            f"openqa-client {_dep_version('openqa_client')}",
        ]
        # ArgumentParser stores the (potentially mocked) sys module so
        # tests / non-default invocations can capture output. Write to
        # the same stream the rest of the parser uses.
        parser.sys.stdout.write("\n".join(lines) + "\n")
        parser.exit(0)


def get_parser(sys) -> ArgumentParser:
    """Creates and configures the argument parser for the application.

    Args:
        sys: The `sys` module, used for stdout/stderr.

    Returns:
        A configured `ArgumentParser` instance.

    """
    parser = ArgumentParser(sys_=sys)
    parser.add_argument(
        "-l", "--location", type=str, help="override config mtui.location"
    )
    parser.add_argument(
        "-t", "--template_dir", type=Path, help="override config mtui.template_dir"
    )
    parser.add_argument(
        "-s",
        "--sut",
        type=SUTParse,
        action="append",
        help="cumulatively override default hosts from template "
        "(format: hostname,hostname2)",
    )
    parser.add_argument(
        "-p",
        "--prerun",
        type=Path,
        help="script with a set of MTUI commands to run at start",
    )
    parser.add_argument(
        "-w",
        "--connection_timeout",
        type=int,
        help="override config mtui.connection_timeout",
    )
    parser.add_argument(
        "-n",
        "--noninteractive",
        action="store_true",
        default=False,
        help="noninteractive update shell",
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
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "-a",
        "--auto-review-id",
        metavar="RequestReviewID",
        type=AutoOBSUpdateID,
        help="OBS request review id (example: SUSE:Maintenance:1:1)",
        dest="update",
    )
    group.add_argument(
        "-k",
        "--kernel-review-id",
        metavar="RequestReviewID",
        type=KernelOBSUpdateID,
        help="OBS kernel/live-patch request review id (example: SUSE:Maintenance:1:1)",
        dest="update",
    )

    return parser
