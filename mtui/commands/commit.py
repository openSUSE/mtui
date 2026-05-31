"""The `commit` command."""

from argparse import REMAINDER
from logging import getLogger

from ..cli.completion import complete_choices
from ..support.misc import requires_update
from ..template import svn_commit_testreport
from . import Command

logger = getLogger("mtui.command.commit")


class Commit(Command):
    """Commits the testing template to SVN.

    This command should be run after testing has finished and the
    template is in its final state.
    """

    command = "commit"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-m", "--msg", action="append", nargs=REMAINDER, help="commit message"
        )

    @requires_update
    def __call__(self) -> None:
        """Executes the `commit` command."""
        checkout = self.metadata.report_wd()

        msg = []
        if self.args.msg:
            msg = ["-m", '"' + " ".join(self.args.msg[0]) + '"']

        try:
            svn_commit_testreport(checkout, self.config.install_logs, msg)
            logger.info("Testreport in: %s", self.metadata.fancy_report_url())
        except Exception:
            logger.exception("committing template.failed")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        """Provides tab completion for the command."""
        return complete_choices([("-m", "--msg")], line, text)
