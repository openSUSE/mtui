from logging import getLogger

from mtui.commands import Command
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.command.testopialist")


class TestopiaList(Command):
    """
    List all Testopia package testcases for the current product.
    If now packages are set, testcases are displayed for the
    current update.
    """

    command = "testopia_list"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "-p",
            "--package",
            nargs="?",
            action="append",
            default=[],
            help="package to display testcases for",
        )

    @requires_update
    def __call__(self):
        self.prompt.ensure_testopia_loaded(*[_f for _f in self.args.package if _f])

        url = self.config.bugzilla_url

        if not self.prompt.testopia.testcases:
            logger.info("no testcases found")

        for tcid, tc in list(self.prompt.testopia.testcases.items()):
            self.display.testopia_list(
                url, tcid, tc["summary"], tc["status"], tc["automated"]
            )

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([("-p", "--package")], line, text)
