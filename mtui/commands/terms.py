# -*- coding: utf-8 -*-

import os

from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices


class Terms(Command):
    """
    Spawn terminal screens to all connected hosts. This command does
    actually just run the available helper scripts.
    If no termname is given, all available terminal scripts are shown.
    """
    command = 'terms'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            'termname',
            nargs='?',
            help='terminal emulator to spawn consoles on')
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        dirname = self.config.datadir
        hosts = [host for host in sorted(self.parse_hosts().names())]

        if self.args.termname:
            if self.args.termname in self.config.termnames:
                filename = 'term.' + self.args.termname + '.sh'
                path = os.path.join(dirname, filename)
                try:
                    check_call([path] + hosts)
                except Exception:
                    self.log.error('running {!s} failed'.format(filename))
                self.log.debug(format_exc())
            else:
                self.log.error('Term script not found')
                self.log.info(
                    'Aviable term scripts: {}'.format(
                        ' '.join(
                            self.config.termnames)))
        else:
            self.println('available terminals scripts:')
            self.println(' '.join(self.config.termnames))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        t = ('-t', '--target')
        a = ()
        for x in state['config'].termnames:
            a += (x,)

        return complete_choices([a, t], line, text, state['hosts'].names())