from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices


class HostsUnlock(Command):
    """ "Unlock refhost
    can be forced by -f/--force parameter
    """

    command = "unlock"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="force unlock - remove locks set by other users or sessions",
        )

        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        hosts = self.parse_hosts()
        hosts.unlock(force=self.args.force)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices(
            [("-f", "--force"), ("-t", "--target")], line, text, state["hosts"].names()
        )
