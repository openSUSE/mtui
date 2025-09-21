"""The `remove_host` command."""

import concurrent.futures

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices


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
        self.targets.pop(target)
        if target in self.metadata.systems:
            del self.metadata.systems[target]

    def __call__(self) -> None:
        """Executes the `remove_host` command."""
        targets = list(self.parse_hosts(enabled=False).keys())
        # for target in targets:
        with concurrent.futures.ThreadPoolExecutor() as executor:
            conn = [executor.submit(self._remove_target, target) for target in targets]
            concurrent.futures.wait(conn, timeout=30)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
