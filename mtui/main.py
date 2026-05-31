"""The main entry point for the mtui application."""

import logging
import sys
from argparse import Namespace
from subprocess import CalledProcessError
from typing import Literal

from .argparse import ArgsParseFailureError
from .args import get_parser
from .colorctl import set_mode as set_color_mode
from .colorlog import create_logger
from .config import Config
from .display import CommandPromptDisplay
from .exceptions import MissingGiteaTokenError
from .messages import MetadataNotLoadedError, SvnCheckoutInterruptedError
from .prompt import CommandPrompt
from .prompter import Prompter
from .systemcheck import detect_system
from .template.nulltestreport import NullTestReport


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
    except ArgsParseFailureError as e:
        return e.status

    set_color_mode(args.color)

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
    config.kernel = False
    config.auto = False

    config.distro, config.distro_ver, config.distro_kernel = detect_system()

    # One Prompter for the whole process: serialises SSH command-timeout
    # prompts across worker threads behind a single lock so they reach
    # the user one at a time (no stdin races, no interleaved prompt text).
    prompter = Prompter()
    prompt = CommandPrompt(config, logger, sys, CommandPromptDisplay, prompter=prompter)
    prompt.interactive = not args.noninteractive
    if args.update:
        if args.update.kind == "kernel":
            config.kernel = True
            config.auto = False
        elif args.update.kind == "auto":
            config.auto = True
            config.kernel = False
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
        except MissingGiteaTokenError:
            # _checkout already logged a user-facing message; just exit.
            return 1

        # An explicitly requested update that could not be loaded falls back
        # to a NullTestReport. The clear "does not exist" message was already
        # logged; quit rather than dropping into an empty interactive session.
        if isinstance(prompt.metadata, NullTestReport):
            return 1

    if args.sut:
        for x in args.sut:
            try:
                prompt.do_add_host(x.print_args())
            except ArgsParseFailureError as e:
                logger.error("failed to add host %s: %s", x, e)

    if args.prerun:
        if args.prerun.is_file():
            prompt.set_cmdqueue(
                [
                    x.rstrip()
                    for x in args.prerun.read_text().splitlines()
                    if not x.startswith("#")
                ]
            )
        else:
            logger.error("Prerun command file %s isn't file", str(args.prerun))

    prompt.cmdloop(intro="Maintenance Test Update Installer")
    return 0
