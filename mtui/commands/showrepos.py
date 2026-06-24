"""The `show_update_repos` command."""

from ..support.misc import requires_update
from . import Command


class Showrepos(Command):
    """Shows the update repositories that are valid for the current update."""

    command = "show_update_repos"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `show_update_repos` command."""
        self.display.list_update_repos(self.metadata.update_repos)
