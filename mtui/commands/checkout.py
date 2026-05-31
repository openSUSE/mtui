"""The `checkout` command."""

from logging import getLogger
from subprocess import check_call

from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.command.checkout")


class Checkout(Command):
    """Updates the template files from SVN."""

    command = "checkout"

    @requires_update
    def __call__(self) -> None:
        """Executes the `checkout` command."""
        try:
            check_call(["svn", "up"], cwd=self.metadata.report_wd())
        except Exception:
            logger.exception("updating template failed")
