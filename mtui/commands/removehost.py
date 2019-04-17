import concurrent.futures
from mtui.commands import Command
from mtui.utils import complete_choices


class RemoveHost(Command):
    """
    Disconnects from host and remove host from list.
    Warning: The host log is purged as well.
    Warning 2: without parameters removes all hosts.
    """

    command = "remove_host"

    @classmethod
    def _add_arguments(cls, parser):

        cls._add_hosts_arg(parser)

        return parser

    def _remove_target(self, target):
        self.targets[target].close()
        self.targets.pop(target)
        if target in self.metadata.systems:
            del self.metadata.systems[target]

    def __call__(self):
        targets = self.parse_hosts(enabled=None).keys()
        # for target in targets:
        with concurrent.futures.ThreadPoolExecutor() as executor:
            conn = [executor.submit(self._remove_target, target) for target in targets]
            concurrent.futures.wait(conn, timeout=30)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
