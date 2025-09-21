"""The `add_host` command."""

import concurrent.futures

from mtui.commands import Command
from mtui.utils import complete_choices


class AddHost(Command):
    """Adds one or more machines to the target host list.

    If no target is specified, all hosts from the test platform are added.
    """

    command = "add_host"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-t",
            "--target",
            action="append",
            help="address of the target host (should be the FQDN)",
        )

    def __call__(self) -> None:
        """Executes the `add_host` command."""
        if not self.args.target:
            for tp in self.metadata.testplatforms:
                self.metadata.refhosts_from_tp(tp)
            self.metadata.connect_targets()
        else:
            with concurrent.futures.ThreadPoolExecutor() as executor:
                connections = [
                    executor.submit(self.metadata.add_target, hostname)
                    for hostname in self.args.target
                ]
                concurrent.futures.wait(connections)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("-t", "--target")], line, text)
