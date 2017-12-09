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
            "-t",
            "--target",
            required=True,
            action='append',
            help='address of the target host (should be the FQDN)')
        return parser

    def run(self):

        for hostname in self.args.target:
            self.metadata.add_target(hostname)

    @staticmethod
    def complete(_, text, line, begidx, endix):
        return complete_choices([('-t', '--target')], line, text)
