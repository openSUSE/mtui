"""The `unlock` command."""

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices


class HostsUnlock(Command):
    """Unlocks a host that was previously locked.

    The unlock can be forced by using the `-f` or `--force` parameter.
    """

    command = "unlock"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="force unlock - remove locks set by other users or sessions",
        )

        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `unlock` command."""
        hosts = self.parse_hosts()
        hosts.unlock(force=self.args.force)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-f", "--force"), ("-t", "--target")], line, text, state["hosts"].names()
        )
