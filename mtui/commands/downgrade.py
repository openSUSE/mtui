# -*- coding: utf-8 -*-

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update
from mtui.messages import NoRefhostsDefinedError

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
        if not targets:
            raise NoRefhostsDefinedError

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

        message = 'done'
        for target in targets.values():
            if message == 'done':
                for package in target.packages.values():
                    if package.before == package.after:
                        message = 'downgrade not completed'
                        break
            else:
                break

        if message == 'done':
            self.log.info(message)
        else:
            self.log.warn(message)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
