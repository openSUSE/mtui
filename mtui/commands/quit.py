"""The `quit`, `exit`, and `EOF` commands."""

import concurrent.futures
from contextlib import suppress
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.concurrency import ContextExecutor
from . import Command

logger = getLogger("mtui.command.quit")

#: How long (in seconds) ``quit`` waits for the parallel host teardown
#: before reporting the remaining hosts as still disconnecting. This does
#: not bound how long ``quit`` itself can block: the executor is still
#: joined on exit, so a straggler past this timeout is merely reported
#: early, not abandoned. Module-level so tests can patch it.
CLOSE_TIMEOUT: float = 45


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
        # Leaving the ``with`` block below still joins every worker thread
        # (``Executor.__exit__`` is ``shutdown(wait=True)``) -- that join is
        # pre-existing behaviour and is left untouched here, so quit keeps
        # blocking until every close has actually finished, however long
        # that takes. Only the *visibility* of the outcome changes: a
        # straggler still running at the timeout gets a "still
        # disconnecting" warning (not a final verdict, since it may yet
        # succeed while the ``with`` block joins it), and is re-checked
        # once the join has completed so an exception raised after the
        # timeout is not silently dropped.
        with ContextExecutor() as executor:
            futures = {
                executor.submit(
                    self._close_target, report.targets, target, args_
                ): target
                for report in self.templates.all()
                for target in set(report.targets)
            }
            done, not_done = concurrent.futures.wait(futures, timeout=CLOSE_TIMEOUT)
            for future in done:
                if (exc := future.exception()) is not None:
                    logger.warning(
                        "failed to disconnect from %s: %s", futures[future], exc
                    )
            for future in not_done:
                logger.warning(
                    "still disconnecting from %s after %s seconds",
                    futures[future],
                    CLOSE_TIMEOUT,
                )
        # The ``with`` block above has just joined every straggler from
        # ``not_done`` (the executor's own ``shutdown(wait=True)`` on
        # exit), so every one of those futures is now guaranteed done.
        # Re-check them: one that went on to raise had its failure reason
        # dropped before this fix (only the timeout warning fired even
        # though quit blocked for the close to finish anyway); one that
        # went on to succeed needs no further warning.
        for future in not_done:
            if (exc := future.exception()) is not None:
                logger.warning("failed to disconnect from %s: %s", futures[future], exc)

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
