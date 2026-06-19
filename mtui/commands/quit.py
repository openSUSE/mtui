"""The `quit`, `exit`, and `EOF` commands."""

import concurrent.futures
from contextlib import suppress

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.concurrency import ContextExecutor
from . import Command


class Quit(Command):
    """Disconnects from all hosts and exits the program.

    If a boot argument is set, the hosts are either rebooted or powered off.
    """

    command = "quit"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "bootarg",
            nargs="?",
            choices=["reboot", "poweroff"],
            help="reboot or poweroff refhosts",
        )

    def _close_target(self, target: str, args) -> None:
        """Closes the connection to a single target host.

        Args:
            target: The hostname of the target host to close.
            args: A list of arguments to pass to the close method.

        """
        self.targets[target].close(*args)
        self.targets.pop(target)

    def __call__(self) -> None:
        """Executes the `quit` command."""
        args_ = [self.args.bootarg] if self.args.bootarg else []

        with ContextExecutor() as executor:
            targets = [
                executor.submit(self._close_target, target, args_)
                for target in set(self.targets)
            ]
            concurrent.futures.wait(targets, timeout=45)

        # FileHistory writes synchronously on each ``append_string``, so the
        # on-disk file is already current. ``flush()`` is the canonical
        # "make sure everything is persisted" call on
        # :class:`prompt_toolkit.history.History`; for ``FileHistory`` it is
        # a no-op today but stays correct if the backend ever buffers.
        with suppress(Exception):
            self.prompt._history.flush()  # noqa: SLF001

        self.sys.exit(0)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("reboot", "poweroff")], line, text)


class QExit(Quit):
    """An alias for the `quit` command."""

    command = "exit"


class DEOF(Quit):
    """An alias for the `quit` command, used for handling `Ctrl-D`."""

    command = "EOF"
