# -*- coding: utf-8 -*-

import os
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update
from mtui.utils import nottest


class TestSuiteList(Command):
    """
    List available testsuites on the target hosts.
    """
    command = 'testsuite_list'

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        targets = self.parse_hosts()

        targets.report_testsuites(
            self.display.testsuite_list,
            self.config.target_testsuitedir)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())


class TestSuiteRun(Command):
    """
    Runs ctcs2 testsuite and saves logs to /var/log/qa/RIDD on the
    target hosts. Results can be submitted with the testsuite_submit
    command.
    """
    command = 'testsuite_run'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument('testsuite', nargs=1, help="testsuite-run command")
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def run(self):
        targets = self.parse_hosts()
        cmd = self.args.testsuite[0]

        if not cmd.startswith('/'):
            cmd = os.path.join(self.config.target_testsuitedir, cmd.strip())

        cmd = 'export TESTS_LOGDIR=/var/log/qa/{}; {}'.format(
            self.metadata.id, cmd)
        name = os.path.basename(cmd).replace('-run', '')

        try:
            targets.run(cmd)
        except KeyboardInterrupt:
            self.log.info('testsuite run canceled')
            return

        for hn, t in targets.items():
            t.report_testsuite_results(self.display.testsuite_run, name)

        self.log.info('done')

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [('-t', '--target'), ],
            line, text, state['hosts'].names())
