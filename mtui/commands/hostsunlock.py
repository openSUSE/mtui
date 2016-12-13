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
        args = self.args

        try:
            hosts = self.hosts.select(args.hosts)
        except ValueError as e:
            self.log.error(e)
            return

        hosts.unlock(force=args.force)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [
                ("-f", "--force")
            ],
            line,
            text,
            state['hosts'].names()
        )
