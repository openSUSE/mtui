"""The `commit` command."""

from argparse import REMAINDER
from logging import getLogger
import subprocess
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices, requires_update

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
            msg = ["-m"] + ['"' + " ".join(self.args.msg[0]) + '"']

        try:
            subprocess.check_call(
                "svn add --force {}".format(str(self.config.install_logs)).split(),
                cwd=checkout,
            )
            if checkout.joinpath("results").exists():
                subprocess.call(
                    "svn add --force {}".format("results").split(),
                    cwd=checkout,
                )
            if checkout.joinpath("checkers.log").exists():
                subprocess.check_call(
                    "svn add --force {}".format("checkers.log").split(),
                    cwd=checkout,
                )
            subprocess.check_call("svn up".split(), cwd=checkout)
            subprocess.check_call("svn ci".split() + msg, cwd=checkout)

            logger.info("Testreport in: {}".format(self.metadata._fancy_report_url()))

        except Exception:
            logger.error("committing template.failed")
            logger.debug(format_exc())

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        """Provides tab completion for the command."""
        return complete_choices([("-m", "--msg")], line, text)
