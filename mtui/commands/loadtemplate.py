# -*- coding: utf-8 -*-

from mtui.template import OBSUpdateID
from mtui.commands import Command
from mtui.utils import prompt_user
from mtui.utils import complete_choices


class LoadTemplate(Command):
    """
    Load QA Maintenance template by RRID identifier. All changes and logs
    from an already loaded template are lost if not saved previously.
    Already connected hosts are kept and extended by the reference hosts
    defined in the template file.
    """
    command = 'load_template'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'update_id',
            nargs=1,
            type=OBSUpdateID,
            help='OBS request id for update')
        parser.add_argument(
            '-c',
            '--clean-hosts',
            dest='chosts',
            action='store_false',
            help='clean up old hosts')
        return parser

    def run(self):
        if self.metadata:
            msg = 'Should i owerwrite already loaded session {}? (y/N) '
            if not prompt_user(
                    msg.format(self.metadata.id),
                    ['y', 'Y', 'yes', 'YES', 'Yes'],
                    self.prompt.interactive):
                return

        re_add = []
        for hostname, target in self.prompt.targets.items():
            re_add.append((hostname, target.system))
            target.close()

        self.prompt.load_update(self.args.update_id[0], autoconnect=True)

        # Reload hosts to which we already have a connection
        # close hosts we are already connected to but add them to the
        # testreport.systems so they get connected to again.
        # This feature comes from pre-1.0 versions.
        # NOTE: the only reason we need to reconnect seems to be that
        # when the L{Target} object is created, it is passed a list of
        # packages, which changes with the testreport change. So this
        # may go away when refactored.

        if self.args.chosts:
            for hostname, system in re_add:
                self.prompt.metadata.add_target(hostname, system)

    @staticmethod
    def complete(_, text, line, begidx, endix):
        return complete_choices(
            [('-c', '--clean-hosts'),
             ("SUSE:Maintenance:", "openSUSE:Maintenance:")],
            line, text)
