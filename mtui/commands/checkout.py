"""The `checkout` command."""

from logging import getLogger
from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import requires_update

logger = getLogger("mtui.command.checkout")


class Checkout(Command):
    """Updates the template files from SVN."""

    command = "checkout"

    @requires_update
    def __call__(self) -> None:
        """Executes the `checkout` command."""
        try:
            check_call("svn up".split(), cwd=self.metadata.report_wd())
        except Exception:
            logger.error("updating template failed")
            logger.debug(format_exc())
