"""The `reload_products` command."""

from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.commands.reload")


class ReloadProducts(Command):
    """Reloads and parses the products on the target reference hosts."""

    command = "reload_products"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `reload_products` command."""
        targets = self.parse_hosts()
        for target in targets:
            targets[target].reload_system()
            logger.info("Reloaded products on refhost %s", target)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
