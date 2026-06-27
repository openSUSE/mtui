"""The `remove_host` command."""

import concurrent.futures

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.concurrency import ContextExecutor
from . import Command


class RemoveHost(Command):
    """Disconnects from a host and removes it from the list.

    Warning:
        The host log is purged as well. If no parameters are provided,
        this command removes all hosts.

    """

    command = "remove_host"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def _remove_target(self, target) -> None:
        """Removes a single target host.

        Args:
            target: The target host to remove.

        """
        self.targets[target].close()
        # Drop the in-process pool-arbitration claim too. close() only removes
        # the remote lock files; the process-global HostArbiter would otherwise
        # keep this host marked busy for the server's lifetime (no unload over
        # MCP, so the template stays loaded). See TestReport.release_pool_claim.
        self.metadata.release_pool_claim(target)
        self.targets.pop(target)
        if target in self.metadata.systems:
            del self.metadata.systems[target]

    def __call__(self) -> None:
        """Executes the `remove_host` command."""
        targets = list(self.parse_hosts(enabled=False).keys())
        # for target in targets:
        with ContextExecutor() as executor:
            conn = [executor.submit(self._remove_target, target) for target in targets]
            concurrent.futures.wait(conn, timeout=30)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
