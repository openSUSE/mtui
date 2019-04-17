

from mtui.commands import Command
from mtui.target.locks import LockedTargets
from mtui.utils import complete_choices
from mtui.utils import requires_update


class SetRepo(Command):
    """
    Add or remove issue repository to/from hosts.
    """

    command = "set_repo"

    @classmethod
    def _add_arguments(cls, parser):

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

        return parser

    @requires_update
    def __call__(self):

        operation = self.args.operation
        hosts = self.parse_hosts()

        with LockedTargets([self.targets[x] for x in hosts]):
            for t in [self.targets[x] for x in hosts]:
                t.set_repo(operation, self.metadata)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target"), ("-A", "--add", "-R", "--remove")],
            line,
            text,
            state["hosts"].names(),
        )
