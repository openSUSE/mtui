"""The `lrun` command."""

from argparse import REMAINDER
from logging import getLogger
from subprocess import check_call

from mtui.argparse import ArgumentParser
from mtui.commands import Command

logger = getLogger("mtui.commands.lrun")


class LocalRun(Command):
    """Runs a command in the local shell.

    The command is run in the current working directory where mtui was
    started, unless chroot to the template directory is enabled.
    """

    command = "lrun"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "command", nargs=REMAINDER, help="command to run on local shell"
        )

    def __call__(self) -> None:
        """Executes the `lrun` command."""
        if not self.args.command:
            logger.error("Missing argument")
            return

        check_call(" ".join(self.args.command), shell=True)
