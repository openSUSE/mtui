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

    @staticmethod
    def _close_target(targets, target: str, args) -> None:
        """Closes the connection to a single target host.

        Args:
            targets: The :class:`HostsGroup` owning the target host.
            target: The hostname of the target host to close.
            args: A list of arguments to pass to the close method.

        """
        targets[target].close(*args)
        targets.pop(target)

    def __call__(self) -> None:
        """Executes the `quit` command."""
        args_ = [self.args.bootarg] if self.args.bootarg else []

        # Release host-arbitration pool claims (in-process ownership + remote
        # pool locks) for every template before closing. No-op without pooling.
        for report in self.templates.all():
            with suppress(Exception):
                report.release_pool_claims()

        # Close every loaded template's host group, not just the active one.
        with ContextExecutor() as executor:
            futures = [
                executor.submit(self._close_target, report.targets, target, args_)
                for report in self.templates.all()
                for target in set(report.targets)
            ]
            concurrent.futures.wait(futures, timeout=45)

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
