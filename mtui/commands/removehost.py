# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices


class RemoveHost(Command):
    """
    Disconnects from host and remove host from list.
    Warning: The host log is purged as well.

    Warning 2: without parameters removes all hosts.
    """
    command = 'remove_host'

    @classmethod
    def _add_arguments(cls, parser):

        cls._add_hosts_arg(parser)

        return parser

    def run(self):
        targets = list(self.parse_hosts(enabled=None).keys())
        for target in targets:
            self.targets[target].close()
            self.targets.pop(target)
            if target in self.metadata.systems:
                del self.metadata.systems[target]

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
