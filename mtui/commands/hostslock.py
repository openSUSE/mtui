"""The `lock` command."""

from argparse import REMAINDER

from mtui.commands import Command
from mtui.utils import complete_choices


class HostLock(Command):
    """Locks a host for exclusive usage.

    This command locks all repository transactions, such as enabling or
    disabling the testing repository on the target hosts. The hosts are
    locked with a timestamp, the UID, and the PID of the session.

    Warning:
        This influences the update process of concurrent instances, so
        use with care.

    Enabled locks are automatically removed when exiting the session.
    To lock the run command on other sessions as well, it's necessary
    to set a comment.
    """

    command = "lock"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)
        parser.add_argument(
            "-c", "--comment", action="append", nargs=REMAINDER, help="lock comment"
        )

    def __call__(self) -> None:
        """Executes the `lock` command."""
        targets = self.parse_hosts()
        comment = "" if not self.args.comment else " ".join(self.args.comment[0])
        targets.lock(comment)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), ("-c", "--comment")],
            line,
            text,
            state["hosts"].names(),
        )
