"""The `checkout` command."""

from logging import getLogger
from subprocess import check_call

from ..cli.completion import complete_choices, template_completion
from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.command.checkout")


class Checkout(Command):
    """Updates the template files from SVN."""

    command = "checkout"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `checkout` command."""
        try:
            check_call(["svn", "up"], cwd=self.metadata.report_wd())
        except Exception:
            logger.exception("updating template failed")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(template_completion(state), line, text)
