"""The `set_repo` command."""

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..hosts.target.locks import LockedTargets, TargetLockedError
from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.command.setrepo")


class SetRepo(Command):
    """Adds or removes an issue repository to or from hosts."""

    command = "set_repo"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        group = parser.add_mutually_exclusive_group(required=True)
        group.add_argument(
            "-A",
            "--add",
            dest="operation",
            action="store_const",
            const="add",
            help="Add issue repos to refhosts",
        )

        group.add_argument(
            "-R",
            "--remove",
            dest="operation",
            action="store_const",
            const="remove",
            help="Remove issue repos from refhosts",
        )

        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `set_repo` command."""
        operation = self.args.operation
        hosts = self.parse_hosts()
        try:
            with LockedTargets([self.targets[x] for x in hosts]):
                for t in [self.targets[x] for x in hosts]:
                    t.repo_manager.set(operation, self.metadata)
        except TargetLockedError as err:
            logger.error("Target locked %s", err)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), ("-A", "--add", "-R", "--remove")],
            line,
            text,
            state["hosts"].names(),
        )
