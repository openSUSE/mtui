# -*- coding: utf-8 -*-

from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices
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
