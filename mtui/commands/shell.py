from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.command.shell")


class Shell(Command):
    """
    Invokes a remote root shell on the target host.
    The terminal size is set once, but isn't adapted on subsequent changes.

    In case of use more host shell is invoked sequentially.
    """

    command = "shell"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        targets = self.parse_hosts()

        logger.debug("Starting shell")

        for target in targets.keys():
            targets[target].shell()

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices([], line, text, state["hosts"].names())
