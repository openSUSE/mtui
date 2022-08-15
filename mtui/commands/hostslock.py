from argparse import REMAINDER

from mtui.commands import Command
from mtui.utils import complete_choices


class HostLock(Command):
    """
    Lock host for exclusive usage. This locks all repository transactions
    like enabling or disabling the testing repository on the target hosts.
    The Hosts are locked with a timestamp, the UID and PID of the session.
    This influences the update process of concurrent instances, use with
    care.

    Enabled locks are automatically removed when exiting the session.
    To lock the run command on other sessions as well, it's necessary to
    set a comment.
    """

    command = "lock"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        cls._add_hosts_arg(parser)
        parser.add_argument(
            "-c", "--comment", action="append", nargs=REMAINDER, help="lock comment"
        )

    def __call__(self):
        targets = self.parse_hosts()
        comment = "" if not self.args.comment else " ".join(self.args.comment[0])
        targets.lock(comment)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target"), ("-c", "--comment")],
            line,
            text,
            state["hosts"].names(),
        )
