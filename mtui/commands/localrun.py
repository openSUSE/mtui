# -*- coding: utf-8 -*-
#
from argparse import REMAINDER
from subprocess import check_call

from mtui.commands import Command


class LocalRun(Command):
    """
    Run command in local shell\n
    Command run in CWD where is mtui started\n
    unless is chroot to template dir enabled.
    """
    command = 'lrun'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "command",
            nargs=REMAINDER,
            help="command to run on local shell")
        return parser

    def run(self):
        if not self.args.command:
            self.log.error("Missing argument")
            return

        check_call(self.args.command)
