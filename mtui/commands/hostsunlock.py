"""The `unlock` command."""

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from . import Command


class HostsUnlock(Command):
    """Unlocks a host that was previously locked.

    By default this removes the zypper/operation lock. Use ``-p``/``--pool``
    to instead remove the host *pool* claim. The unlock can be forced by
    using the ``-f``/``--force`` parameter, which also removes locks set by
    other users or sessions (or, with ``--pool``, by other templates).
    """

    command = "unlock"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="force unlock - remove locks set by other users or sessions",
        )
        parser.add_argument(
            "-p",
            "--pool",
            action="store_true",
            help="remove the pool claim instead of the zypper/operation lock",
        )

        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    def __call__(self) -> None:
        """Executes the `unlock` command."""
        hosts = self.parse_hosts()
        if self.args.pool:
            hosts.pool_unlock(force=self.args.force)
        else:
            hosts.unlock(force=self.args.force)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-f", "--force"),
                ("-p", "--pool"),
                ("-t", "--target"),
                *template_completion(state),
            ],
            line,
            text,
            state["hosts"].names(),
        )
