"""The `run` command."""

from argparse import REMAINDER
from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.messages import NoRefhostsDefinedError
from mtui.target.locks import LockedTargets, TargetLockedError
from mtui.utils import complete_choices, page

logger = getLogger("mtui.command.run")


class Run(Command):
    """Runs a command on a specified host or on all enabled targets.

    The command timeout is set to 5 minutes, which means that if there is
    no output on stdout or stderr for 5 minutes, a timeout exception is
    thrown.

    The commands are run in parallel on every target or in serial mode
    when set with "set_host_state". After the call returns, the output
    (including the return code) of each host is shown on the console.

    Note:
        No interactive commands can be run with this procedure.
    """

    command = "run"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "command", nargs=REMAINDER, help="Command to run on refhost"
        )
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `run` command."""
        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        command: str = ""

        for i in self.args.command:
            command += i + " "

        command = command.rstrip(" ")
        try:
            with LockedTargets(list(targets.values())):
                try:
                    targets.run(command)
                except KeyboardInterrupt:
                    return

                output = []

                for target in targets:
                    output.append(
                        "{!s}:-> {!s} [{!s}]".format(
                            target, targets[target].lastin(), targets[target].lastexit()
                        )
                    )
                    for line in targets[target].lastout().split("\n"):
                        output.append(line)
                    if targets[target].lasterr():
                        output.append("stderr:")
                        for line in targets[target].lasterr().split("\n"):
                            output.append(line)

        except TargetLockedError as e:
            logger.error("Target %s", e)
            return

        page(output, self.prompt.interactive)
        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
