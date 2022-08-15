from logging import getLogger
from os import getenv
from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices_filelist, requires_update

logger = getLogger("mtui.command.edit")


class Edit(Command):
    """
    Edit the testing template or local file. To edit template call
    edit without parameters.
    The evironment variable $EDITOR is processed to find the prefered
    editor. If $EDITOR is empty, "vim" is set as default.
    """

    command = "edit"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("filename", nargs="?", type=str, help="file to edit")
        return parser

    @requires_update
    def _template(self):
        return self.metadata.path

    def __call__(self):
        path = self.args.filename if self.args.filename else self._template()

        editor = getenv("EDITOR", "vim")

        try:
            logger.debug("call {!s} on {!s}".format(editor, path))
            check_call([editor, path])
        except Exception:
            logger.error("failed to run {!s}".format(editor))
            logger.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices_filelist([], line, text)
