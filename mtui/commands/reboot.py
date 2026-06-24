"""The `reboot` command."""

from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import NoRefhostsDefinedError
from . import Command

logger = getLogger("mtui.commands.reboot")


class Reboot(Command):
    """Reboots reference hosts and reconnects once they are back up.

    Reboots all connected reference hosts, or only those given with
    `-t`/`--target`. The reboot is dispatched without waiting (the SSH
    connection is expected to drop), then mtui reconnects automatically
    with retries and backoff while each host comes back. Works for both
    transactional and non-transactional hosts.

    While testing a Product Increment, the per-host testing lock is
    re-applied after the reboot (a reboot clears `/var/lock`), so it is
    not lost.
    """

    command = "reboot"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    def __call__(self) -> None:
        """Executes the `reboot` command."""
        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        # Re-assert an active Product Increment testing lock after the
        # reboot: rebooting clears /var/lock (tmpfs), so the lock must be
        # written again to survive. ``lock_comment`` is empty unless a PI
        # assignment is active, in which case no relock happens.
        targets.reboot(relock_comment=self.metadata.lock_comment)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
