from argparse import Namespace
import logging
from subprocess import CalledProcessError
import sys
from typing import Literal

from mtui.args import get_parser
from mtui.config import Config
from mtui.display import CommandPromptDisplay
from mtui.messages import SvnCheckoutInterruptedError
from mtui.prompt import CommandPrompt
from mtui.systemcheck import detect_system

from .argparse import ArgsParseFailure
from .colorlog import create_logger


def main() -> int:
    logger = create_logger("mtui")

    p = get_parser(sys)
    try:
        args = p.parse_args(sys.argv[1:])
    except ArgsParseFailure as e:
        return e.status

    if args.noninteractive and not args.prerun:
        logger.error("--noninteractive makes no sense without --prerun")
        p.print_help()
        sys.exit(1)

    cfg = Config(args.config)

    sys.exit(run_mtui(cfg, logger, args))


def run_mtui(config: Config, logger: logging.Logger, args: Namespace) -> Literal[0, 1]:
    if args.debug:
        logger.setLevel(level=logging.DEBUG)

    config.merge_args(args)
    config.kernel = False  # type: ignore
    config.auto = False  # type: ignore

    config.distro, config.distro_ver, config.distro_kernel = detect_system()  # type: ignore

    prompt = CommandPrompt(config, logger, sys, CommandPromptDisplay)
    if args.update:
        if args.update.kind == "kernel":
            config.kernel = True  # type: ignore
            config.auto = False  # type: ignore
        elif args.update.kind == "auto":
            config.auto = True  # type: ignore
            config.kernel = False  # type: ignore
        else:
            pass
        try:
            prompt.load_update(args.update, autoconnect=not bool(args.sut))
        except (SvnCheckoutInterruptedError, CalledProcessError) as e:
            logger.error(e)
            return 1

    if args.sut:
        for x in args.sut:
            try:
                prompt.do_add_host(x.print_args())
            except BaseException:
                pass

    prompt.interactive = not args.noninteractive

    if args.prerun:
        prompt.set_cmdqueue(
            [x.rstrip() for x in args.prerun.readlines() if not x.startswith("#")]
        )

    prompt.cmdloop(intro="Maintenance Test Update Installer")
    return 0
