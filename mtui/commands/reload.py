from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.commands.reload")


class ReloadProducts(Command):
    """Reload and parse products on target refhosts"""

    command = "reload_products"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        targets = self.parse_hosts()
        for target in targets:
            system = targets[target]._parse_system()
            targets[target].system = system
            logger.info("Reloaded products on refhost %s", target)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
