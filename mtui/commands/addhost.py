# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices


class AddHost(Command):
    """
    Adds another machine to the target host list. The system type needs
    to be specified as well.
    """
    command = 'add_host'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-s",
            "--system",
            nargs=1,
            required=True,
            help="system type, ie. sles11sp1-i386")
        parser.add_argument(
            "-t",
            "--target",
            required=True,
            action='append',
            help='address of the target host (should be the FQDN)')
        return parser

    def run(self):

        for hostname in self.args.target:
            self.metadata.add_target(hostname, self.args.system[0])

    @staticmethod
    def complete(_, text, line, begidx, endix):
        return complete_choices(
            [('-t', '--target'),
             ('-s', '--system')],
            line, text)
