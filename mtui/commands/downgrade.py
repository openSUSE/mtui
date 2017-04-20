# -*- coding: utf-8 -*-

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update


class Downgrade(Command):
    """
    Downgrades all related packages to the last released version
    """

    command = 'downgrade'

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def run(self):

        targets = self.parse_hosts()

        self.log.info('Downgrading')

        try:
            self.metadata.perform_downgrade(targets)
        except KeyboardInterrupt:
            self.log.info('downgrade process canceled')
            return
        except Exception:
            self.log.critical('failed to downgrade target systems')
            self.log.debug(format_exc())
            return

        self.log.info('done')

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
