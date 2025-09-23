"""The main entry point for the mtui application."""

import logging
import sys
from argparse import Namespace
from subprocess import CalledProcessError
from typing import Literal

from .argparse import ArgsParseFailure
from .args import get_parser
from .colorlog import create_logger
from .config import Config
from .display import CommandPromptDisplay
from .messages import MetadataNotLoadedError, SvnCheckoutInterruptedError
from .prompt import CommandPrompt
from .systemcheck import detect_system


def main() -> int:
    """The main entry point for the mtui application.

    This function handles command-line argument parsing, configuration
    loading, and starting the command prompt.

    Returns:
        The exit code of the application.
    """
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
    """Initializes and runs the mtui command prompt.

    Args:
        config: The application configuration.
        logger: The logger instance.
        args: The parsed command-line arguments.

    Returns:
        0 on success, 1 on failure.
    """
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
        except (
            SvnCheckoutInterruptedError,
            CalledProcessError,
            MetadataNotLoadedError,
        ) as e:
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
