# -*- coding: utf-8 -*-

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update


class Update(Command):
    """
    Applies the testing update to the target hosts. While updating the
    machines, the pre-, post- and compare scripts are run before and
    after the update process. If the update adds new packages to the
    channel, the "--newpackage" parameter triggers the package installation
    right after the update. To skip the preparation procedure, append
    "--noprepare" to the argument list.
    """

    command = 'update'

    @classmethod
    def _add_arguments(cls, parser):

        parser.add_argument(
            "--newpackage",
            action='store_const',
            const='newpackage',
            help="Install new packages after update")
        parser.add_argument(
            "--noprepare",
            action='store_const',
            const='noprepare',
            help="Skip prepare procedure")
        parser.add_argument(
            "--noscript",
            action='store_const',
            const='noscript',
            help="Don't run pre and post scripts")

        cls._add_hosts_arg(parser)

        return parser

    @requires_update
    def run(self):

        self.log.info('Updating')

        targets = self.parse_hosts()

        params = []
        params.append(self.args.newpackage)
        params.append(self.args.noprepare)
        params.append(self.args.noscript)

        try:
            self.metadata.perform_update(targets, params)

        except Exception:
            self.log.critical('failed to update target systems')
            self.log.debug(format_exc())
            self.prompt.notify_user(
                'updating {!s} failed'.format(self.prompt.session),
                'stock_dialog-error')
            raise

        except KeyboardInterrupt:
            self.log.info('update process canceled')
            return

        self.prompt.notify_user(
            'updating {!s} finished'.format(
                self.prompt.session))
        self.log.info('done')

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'),
             ('--noprepare',),
             ('--newpackage',),
             ('--noscript',), ],
             line, text, state['hosts'].names())
