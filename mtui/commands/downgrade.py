"""The `downgrade` command."""

from logging import getLogger
from traceback import format_exc

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import NoRefhostsDefinedError
from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.command.downgrade")


class Downgrade(Command):
    """Downgrades all related packages to the last released version.

    Warning:
        This command cannot work for new packages.

    """

    command = "downgrade"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `downgrade` command."""
        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        logger.info("Downgrading")

        try:
            self.metadata.perform_downgrade(targets)
        except KeyboardInterrupt:
            logger.info("downgrade process canceled")
            return
        except Exception:
            logger.critical("failed to downgrade target systems")
            logger.debug(format_exc())
            return

        message = "done"
        for target in targets.values():
            target.query_versions()
            if message == "done":
                for package in target.packages:
                    target.packages[package].before = target.packages[package].after
                    target.packages[package].after = target.packages[package].current
                    if (
                        target.packages[package].after is not None
                        and target.packages[package].before is not None
                        and target.packages[package].before
                        == target.packages[package].after
                    ):
                        message = "downgrade not completed"
                        break
            else:
                break

        if message == "done":
            logger.info(message)
        else:
            logger.warning(message)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
