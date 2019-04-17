# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update


class TestopiaList(Command):
    """
    List all Testopia package testcases for the current product.
    If now packages are set, testcases are displayed for the
    current update.
    """

    command = "testopia_list"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-p",
            "--package",
            nargs="?",
            action="append",
            default=[],
            help="package to display testcases for",
        )

        return parser

    @requires_update
    def __call__(self):
        self.prompt.ensure_testopia_loaded(*[_f for _f in self.args.package if _f])

        url = self.config.bugzilla_url

        if not self.prompt.testopia.testcases:
            self.log.info("no testcases found")

        for tcid, tc in list(self.prompt.testopia.testcases.items()):
            self.display.testopia_list(
                url, tcid, tc["summary"], tc["status"], tc["automated"]
            )

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([("-p", "--package")], line, text)
