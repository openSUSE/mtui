"""The `list_products` command."""

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices


class ListProducts(Command):
    """Prints the installed products on the reference hosts."""

    command = "list_products"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `list_products` command."""
        targets = self.parse_hosts(enabled=False)
        targets.report_products(self.display.list_products)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
