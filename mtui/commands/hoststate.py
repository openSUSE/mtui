from logging import getLogger

from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.command.hoststate")


class HostState(Command):
    """
    Sets the host state to "Enabled", "Disabled" or "Dryrun".
    A host set to "Enabled" runs all issued commands while a "Disabled" host
    or a host set to "Dryrun" doesn't run any command on the host.

    The difference between "Disabled" and "Dryrun" is that on "Dryrun"
    hosts the issued commands are printed to the console while "Disabled"
    doesn't print anything.

    Additionally, the execution mode of each host could be set
    to "parallel" (default) or "serial".

    All commands which are designed to run in parallel are influenced
    by this option (like to run command)
    """

    command = "set_host_state"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "state",
            nargs=1,
            choices=["parallel", "serial", "dryrun", "disabled", "enabled"],
        )
        cls._add_hosts_arg(parser)
        return parser

    def __call__(self):
        targets = self.parse_hosts(enabled=False)
        state = self.args.state[0]
        if state in ["serial", "parallel"]:
            if state == "serial":
                exclusive = True
            elif state == "parallel":
                exclusive = False
            for target in targets:
                logger.info(f"Setting host {target} mode to {state}")
                targets[target].exclusive = exclusive
        else:
            for target in targets:
                logger.info(f"Setting host {target} state to {state}")
                targets[target].state = state

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        choices = [
            ("-t", "--target"),
            ("parallel", "serial", "dryrun", "enabled", "disabled"),
        ]
        return complete_choices(choices, line, text, state["hosts"].names())
