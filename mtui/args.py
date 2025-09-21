"""Defines the command-line arguments for the mtui tool."""

from argparse import FileType
from pathlib import Path

from mtui import __version__

from .argparse import ArgumentParser
from .types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from .utils import SUTParse


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
        help="cumulatively override default hosts from template \n"
        "format: hostname,hostname2",
    )
    parser.add_argument(
        "-p",
        "--prerun",
        type=FileType("r"),
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
        action="version",
        version="{}".format(__version__),
        help="print version and exit",
    )
    parser.add_argument(
        "-c", "--config", type=Path, default=None, help="Override default config path"
    )
    parser.add_argument("--smelt_api", type=str, help="SMELT graphQL API endpoint")
    parser.add_argument("-g", "--gitea_token", type=str, help="Gitea Access Token")
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "-a",
        "--auto-review-id",
        metavar="RequestReviewID",
        type=AutoOBSUpdateID,
        help="OBS request review id\nexample: SUSE:Maintenance:1:1",
        dest="update",
    )
    group.add_argument(
        "-k",
        "--kernel-review-id",
        metavar="RequestReviewID",
        type=KernelOBSUpdateID,
        help="OBS kernel/live-patch request review id\nexample: SUSE:Maintenance:1:1",
        dest="update",
    )

    return parser
