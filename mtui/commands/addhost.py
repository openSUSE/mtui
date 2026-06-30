"""The `add_host` command."""

import concurrent.futures
from logging import getLogger

from ..cli.completion import complete_choices, template_completion
from ..support.concurrency import ContextExecutor
from ..types import Workflow
from . import Command

logger = getLogger("mtui.commands.addhost")


class AddHost(Command):
    """Adds one or more machines to the target host list.

    If no target is specified, all hosts from the test platform are added.
    """

    command = "add_host"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-t",
            "--target",
            action="append",
            help="address of the target host (should be the FQDN)",
        )
        parser.add_argument(
            "-k",
            "--keep-mode",
            action="store_true",
            help="do not switch to the manual workflow when in automatic mode",
        )
        cls._add_template_arg(parser)

    def __call__(self) -> None:
        """Executes the `add_host` command."""
        # Running add_host is a manual action. If the session is still in
        # automatic mode the user almost certainly meant to test manually
        # (and just forgot to switch), so move to the manual workflow --
        # unless --keep-mode was given.
        if self.metadata.workflow is Workflow.AUTO and not self.args.keep_mode:
            logger.info("add_host: switching from automatic to manual workflow")
            self.metadata.workflow = Workflow.MANUAL
            self.prompt.set_prompt()

        if not self.args.target:
            for tp in self.metadata.testplatforms:
                self.metadata.refhosts_from_tp(tp)
            self.metadata.connect_targets()
        else:
            with ContextExecutor() as executor:
                connections = [
                    executor.submit(self.metadata.add_target, hostname)
                    for hostname in self.args.target
                ]
                concurrent.futures.wait(connections)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), ("-k", "--keep-mode"), *template_completion(state)],
            line,
            text,
        )
