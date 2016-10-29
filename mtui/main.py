# -*- coding: utf-8 -*-

from __future__ import absolute_import

import sys
import logging
from argparse import FileType
from subprocess import CalledProcessError

from .argparse import ArgumentParser
from .argparse import ArgsParseFailure
from mtui.log import create_logger
from mtui.config import Config
from mtui.prompt import CommandPrompt
from mtui.display import CommandPromptDisplay
from mtui.template import OBSUpdateID
from mtui.messages import SvnCheckoutInterruptedError
from mtui import __version__


def get_parser(sys):
    """
    :covered-by: tests.test_main.test_argparser_*
    """

    p = ArgumentParser(sys_=sys)
    p.add_argument(
        '-l', '--location',
        type=str,
        help='override config mtui.location'
    )
    p.add_argument(
        '-a', '--autoadd',
        type=str,
        action='append',
        help='autoconnect to hosts defined by cumulative attributes'
    )
    p.add_argument(
        '-t', '--template_dir',
        type=str,
        help='override config mtui.template_dir'
    )
    g = p.add_mutually_exclusive_group()
    g.add_argument(
        '-r', '--review-id',
        type=OBSUpdateID,
        help='OBS request review id\nexample: SUSE:Maintenance:1:1'
    )
    p.add_argument(
        '-s', '--sut',
        type=str,
        action='append',
        help='cumulatively override default hosts from template \n'
        "format: hostname,system"
    )
    p.add_argument(
        '-p', '--prerun',
        type=FileType('r'),
        help='script with a set of MTUI commands to run at start'
    )
    p.add_argument(
        '-w', '--connection_timeout',
        type=int,
        help='override config mtui.connection_timeout'
    )
    p.add_argument(
        '-n', '--noninteractive',
        action='store_true',
        default=False,
        help='noninteractive update shell'
    )
    p.add_argument(
        '-d', '--debug',
        action='store_true',
        default=False,
        help='enable debugging output'
    )
    p.add_argument(
        '-V', '--version',
        action='store_true',
        default=False,
        help='print version and exit'
    )

    return p


def main():
    logger = create_logger()
    cfg = Config(logger)
    sys.exit(run_mtui(
        sys, cfg, logger, CommandPrompt, CommandPromptDisplay
    ))


def run_mtui(
    sys, config, log, Prompt, Display
):
    p = get_parser(sys)
    try:
        args = p.parse_args(sys.argv[1:])
    except ArgsParseFailure as e:
        return e.status

    if args.noninteractive and not args.prerun:
        log.error("--noninteractive makes no sense without --prerun")
        p.print_help()
        return 1

    if args.version:
        sys.stdout.write(__version__ + "\n")
        return 0

    if args.debug:
        log.setLevel(level=logging.DEBUG)

    config.merge_args(args)

    update = args.review_id

    prompt = Prompt(config, log, sys, Display)
    if update:
        try:
            prompt.load_update(update, autoconnect=not bool(args.sut))
        except (SvnCheckoutInterruptedError, CalledProcessError) as e:
            log.error(e)
            return 1

    if args.sut:
        for x in args.sut:
            prompt.do_add_host(x)

    if args.autoadd:
        prompt.do_autoadd(" ".join(args.autoadd))

    prompt.interactive = not args.noninteractive

    if args.prerun:
        prompt.set_cmdqueue([x.rstrip()
                             for x in args.prerun.readlines()
                             if not x.startswith('#')])

    prompt.cmdloop()
    return 0
