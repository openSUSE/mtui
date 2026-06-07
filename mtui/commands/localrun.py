"""The `lrun` command."""

import subprocess
from argparse import REMAINDER
from logging import getLogger

from ..cli.argparse import ArgumentParser
from . import Command

logger = getLogger("mtui.commands.lrun")


class LocalRun(Command):
    """Runs a command in the local shell.

    The command is run in the current working directory where mtui was
    started, unless chroot to the template directory is enabled.

    When the surrounding prompt is interactive (a human at the REPL), the
    child process inherits the terminal so output streams live. Under a
    non-interactive prompt (the MCP server, headless callers), stdout and
    stderr are captured and re-emitted through ``self.sys`` so the caller
    can see them; on non-zero exit the real return code is propagated via
    ``self.sys.exit`` instead of being collapsed to a generic failure.
    """

    command = "lrun"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "command", nargs=REMAINDER, help="command to run on local shell"
        )

    def __call__(self) -> None:
        """Executes the `lrun` command."""
        if not self.args.command:
            logger.error("Missing argument")
            return

        cmd = " ".join(self.args.command)

        # Default to the safe (current) streaming behaviour if a caller
        # forgets to set ``interactive`` on the prompt.
        if getattr(self.prompt, "interactive", True):
            subprocess.check_call(cmd, shell=True)
            return

        result = subprocess.run(
            cmd,
            shell=True,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.stdout:
            self.sys.stdout.write(result.stdout)
        if result.stderr:
            self.sys.stderr.write(result.stderr)
        if result.returncode != 0:
            self.sys.exit(result.returncode)
