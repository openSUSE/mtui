# -*- coding: utf-8 -*-

from argparse import REMAINDER

from mtui.commands import Command
from mtui.utils import complete_choices, page
from mtui.target.locks import LockedTargets
from mtui.messages import NoRefhostsDefinedError

class Run(Command):
    """
    Runs a command on a specified host or on all enabled targets.
    The command timeout is set to 5 minutes  which means, if there's no output
    on stdout or stderr for 5 minutes, a timeout exception is thrown.
    The commands are run in parallel on every target or in serial mode when
    set with "set_host_state". After the call returned, the output (including
    the return code) of each host is shown on the console. Please be aware that
    no interactive commands can be run with this procedure.
    """
    command = 'run'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "command",
            nargs=REMAINDER,
            help="Command to run on refhost")
        cls._add_hosts_arg(parser)
        return parser

    def run(self):

        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        command = ''

        for i in self.args.command:
            command += i + ' '

        command = command.rstrip(' ')

        with LockedTargets(list(targets.values())):
            try:
                targets.run(command)
            except KeyboardInterrupt:
                return

            output = []

            for target in targets:
                output.append(
                    '{!s}:-> {!s} [{!s}]'.format(
                        target,
                        targets[target].lastin(),
                        targets[target].lastexit()))
                list(map(output.append, targets[target].lastout().split('\n')))
                if targets[target].lasterr():
                    list(map(output.append, ['stderr:'] +
                        targets[target].lasterr().split('\n')))

        page(output, self.prompt.interactive)
        self.log.info('done')

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
