# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices


class HostsUnlock(Command):
    command = 'unlock'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-f',
            '--force',
            action='store_true',
            help='force unlock - remove locks set by other users or sessions')

        cls._add_hosts_arg(parser)
        return parser

    def run(self):

        hosts = self.parse_hosts()
        hosts.unlock(force=self.args.force)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [
                ("-f", "--force")
                ("-t", "--target"),
            ],
            line,
            text,
            state['hosts'].names()
        )
