"""The `set_host_state` command."""

from logging import getLogger

from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.command.hoststate")


class HostState(Command):
    """Sets the state and execution mode of a host.

    A host can be in one of the following states:
    - "Enabled": Runs all issued commands.
    - "Disabled": Does not run any commands.
    - "Dryrun": Does not run any commands, but prints them to the console.

    The execution mode can be either "parallel" (default) or "serial".
    """

    command = "set_host_state"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "state",
            nargs=1,
            choices=["parallel", "serial", "dryrun", "disabled", "enabled"],
        )
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `set_host_state` command."""
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
        """Provides tab completion for the command."""
        choices = [
            ("-t", "--target"),
            ("parallel", "serial", "dryrun", "enabled", "disabled"),
        ]
        return complete_choices(choices, line, text, state["hosts"].names())
