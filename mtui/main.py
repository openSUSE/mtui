# -*- coding: utf-8 -*-

from __future__ import absolute_import

import os, glob, stat
import sys
import errno
import getopt
import logging
import shutil
from traceback import format_exc
import warnings
from argparse import FileType
from subprocess import CalledProcessError

from .argparse import ArgumentParser
from .argparse import ArgsParseFailure
from mtui import log as _crap_imported_for_side_effects
from mtui.config import config
from mtui.prompt import CommandPrompt
from mtui.template import OBSUpdateID
from mtui.template import SwampUpdateID
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
        '-m', '--md5',
        type=SwampUpdateID,
        help='md5 update identifier'
    )
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
        help= 'override config mtui.connection_timeout'
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
    sys.exit(run_mtui(
      sys
    , config
    , logging.getLogger('mtui')
    , CommandPrompt
    ))

def run_mtui(
  sys
, config
, log
, Prompt
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

    update = args.md5 or args.review_id

    prompt = Prompt(config, log)
    if update:
        try:
            prompt.load_update(update, not bool(args.sut))
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
