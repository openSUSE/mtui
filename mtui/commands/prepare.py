# -*- coding: utf-8 -*-

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import requires_update, complete_choices
from mtui.messages import NoRefhostsDefinedError

class Prepare(Command):
    """
    Installs missing or outdated packages from the UPDATE repositories.
    This is also run by the update procedure before applying the updates.
    """
    command = 'prepare'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-f',
            '--force',
            action='store_const',
            const='force',
            help="force package installation")
        parser.add_argument(
            '-i',
            '--installed',
            action='store_const',
            const='installed',
            help="prepare only installed packages")
        parser.add_argument(
            '-u',
            '--update',
            action='store_const',
            const='testing',
            help="enable test update repositories")
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def run(self):

        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        params = []
        params.append(self.args.force)
        params.append(self.args.installed)
        params.append(self.args.update)

        self.log.info('preparing')

        try:
            self.metadata.perform_prepare(
                targets,
                force='force' in params,
                installed_only='installed' in params,
                testing='testing' in params)
        except KeyboardInterrupt:
            self.log.info("preparation process canceled")
            return False
        except Exception:
            self.log.critical("Failed to prepare systems")
            self.log.debug(format_exc())
            return False

        self.log.info('done')

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'),
             ('-i', '--installed'),
             ('-f', '--force'),
             ('-u', '--update')],
            line, text, state['hosts'].names())
