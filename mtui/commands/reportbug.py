import errno
from logging import getLogger
import subprocess
from time import sleep

from mtui import messages
from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices

logger = getLogger("mtui.commands.reportbug")


class ReportBug(Command):
    """
    Open mtui bugzilla with fields common for all mtui bugs prefilled
    """

    command = "report-bug"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument(
            "-p",
            "--print-url",
            help="just print url to the stdout",
            action="store_true",
        )

    def __call__(self) -> None:
        url = self.config.report_bug_url

        if self.args.print_url:
            self.println(url)
            return

        args = ["xdg-open", url]
        try:
            p = subprocess.Popen(
                args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
        except OSError as e:
            if e.errno == errno.ENOENT:
                raise messages.SystemCommandNotFoundError(args[0])
            else:
                raise

        # xdg-open starts the appropriate command and waits for it
        # to exit.
        # Assuming to propagate it's return code to the caller.
        # However we don't want to block the mtui prompt.

        sleep(1)
        # So we wait a second to let the xdg-open do it's forks and
        # execs
        rc = p.poll()
        if rc is None:
            # and if by now it did not return, we'll assume it done it's
            # job successfully and kill it, leaving it's child still
            # running reparented to init.
            p.kill()
        elif rc != 0:
            # otherwise raise error if ended with non-zero
            raise messages.SystemCommandError(rc, args)
        else:
            # otherwise log a debug message as this state is expected
            # not to happen and we might be interested in knowing about
            # when it does.
            logger.debug(messages.UnexpectedlyFastCleanExitFromXdgOpen())

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices([("-p", "--print-url")], line, text)
