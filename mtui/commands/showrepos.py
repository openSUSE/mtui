"""The `show_update_repos` command."""

from mtui.commands import Command
from mtui.utils import requires_update


class Showrepos(Command):
    """Shows the update repositories that are valid for the current update."""

    command = "show_update_repos"

    @requires_update
    def __call__(self) -> None:
        """Executes the `show_update_repos` command."""
        self.display.list_update_repos(self.metadata.update_repos, self.metadata.id)
