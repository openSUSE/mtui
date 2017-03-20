# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices


class Shell(Command):
    """
    Invokes a remote root shell on the target host.
    The terminal size is set once, but isn't adapted on subsequent changes.
    """
    command = 'shell'

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        targets = self.parse_hosts()

        self.log.debug('Starting shell')

        for target in list(targets.keys()):
            targets[target].shell()

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices([], line, text, state['hosts'].names())
