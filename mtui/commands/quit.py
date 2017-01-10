# -*- coding: utf-8 -*-

import readline

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import prompt_user


class Quit(Command):
    """
    Disconnects from all hosts and exits the programm. If a bootarg
    argument is set, the hosts are either rebooted or powered off.
    The tester is asked to save the XML log when exiting MTUI.
    """
    command = 'quit'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'bootarg',
            nargs='?',
            choices=[
                'reboot',
                'poweroff'],
         help='reboot or poweroff refhosts')
        return parser

    def run(self):

        if not prompt_user('save log? (Y,n) ',
                           ['n', 'no', 'N', 'No', 'NO', 'nein', 'ne'],
                           self.prompt.interactive):
            self.prompt._do_save_impl()

        args_ = [self.args.bootarg] if self.args.bootarg else []

        for x in set(self.targets):
            self.targets[x].close(*args_)
            self.targets.pop(x)

        try:
            readline.write_history_file(
                '{!s}/.mtui_history'.format(self.prompt.homedir))
        except:
            pass

        self.sys.exit(0)

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([('reboot', 'poweroff')], line, text)


class QExit(Quit):
    command = 'exit'


class DEOF(Quit):
    command = 'EOF'
