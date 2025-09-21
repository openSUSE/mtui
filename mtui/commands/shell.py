"""The `shell` command."""

from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.command.shell")


class Shell(Command):
    """Invokes a remote root shell on the target host.

    The terminal size is set once, but is not adapted on subsequent
    changes. If multiple hosts are specified, a shell is invoked
    sequentially on each host.
    """

    command = "shell"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `shell` command."""
        targets = self.parse_hosts()

        logger.debug("Starting shell")

        for target in targets.keys():
            targets[target].shell()

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([], line, text, state["hosts"].names())
