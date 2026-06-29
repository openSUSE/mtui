"""The `run` command."""

import shlex
from argparse import REMAINDER
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..cli.term import page
from ..hosts.target.locks import LockedTargets, TargetLockedError
from ..support.messages import NoRefhostsDefinedError
from . import Command

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
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "command", nargs=REMAINDER, help="Command to run on refhost"
        )
        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    def __call__(self) -> None:
        """Executes the `run` command."""
        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        # Quote each argument so a single token that contains shell
        # metacharacters (e.g. ``sh -c "VAR=x; echo $VAR"`` or ``$(...)``)
        # survives the trip to the remote shell intact. A plain space-join
        # would let the remote shell re-split the script body, dropping the
        # quoting -- so ``sh -c "a; b"`` ran as ``sh -c a`` with ``; b`` leaking
        # into the outer shell, and ``$VAR``/``$(...)`` expanded empty.
        command = shlex.join(self.args.command)
        output: list[str] = []
        try:
            with LockedTargets(list(targets.values())):
                try:
                    targets.run(command)
                except KeyboardInterrupt:
                    return

                for target in targets:
                    output.append(
                        f"{target!s}:-> {targets[target].lastin()!s} [{targets[target].lastexit()!s}]"
                    )
                    output.extend(targets[target].lastout().split("\n"))
                    if targets[target].lasterr():
                        output.append("stderr:")
                        output.extend(targets[target].lasterr().split("\n"))

        except TargetLockedError as e:
            logger.error("Target %s", e)
            return

        page(output, self.prompt.interactive, writer=self.display.println)
        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), *template_completion(state)],
            line,
            text,
            state["hosts"].names(),
        )
