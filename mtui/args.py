
from argparse import FileType

from .argparse import ArgumentParser
from mtui.template.updateid import OBSUpdateID
from mtui.utils import SUTParse

from mtui import __version__


def get_parser(sys):
    parser = ArgumentParser(sys_=sys)
    parser.add_argument(
        "-l", "--location", type=str, help="override config mtui.location"
    )
    parser.add_argument(
        '-t', '--template_dir',
        type=str,
        help='override config mtui.template_dir'
    )
    parser.add_argument(
        "-r",
        "--review-id",
        type=OBSUpdateID,
        help="OBS request review id\nexample: SUSE:Maintenance:1:1",
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

    return parser
