"""The `edit` command."""

from logging import getLogger
from os import getenv
from subprocess import check_call
from traceback import format_exc

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices_filelist, requires_update

logger = getLogger("mtui.command.edit")


class Edit(Command):
    """Edits the testing template or a local file.

    To edit the template, run the command without any parameters. The
    environment variable $EDITOR is used to determine the preferred

    editor. If $EDITOR is not set, "vim" is used by default.
    """

    command = "edit"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument("filename", nargs="?", type=str, help="file to edit")

    @requires_update
    def _template(self):
        """Returns the path to the testing template."""
        return self.metadata.path

    def __call__(self) -> None:
        """Executes the `edit` command."""
        path = self.args.filename if self.args.filename else self._template()

        editor = getenv("EDITOR", "vim")

        try:
            logger.debug("call %s on %s", editor, path)
            check_call([editor, path])
        except Exception:
            logger.error("failed to run %s", editor)
            logger.debug(format_exc())

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices_filelist([], line, text)
