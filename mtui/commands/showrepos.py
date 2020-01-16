from mtui.commands import Command
from mtui.utils import requires_update


class Showrepos(Command):
    """
    Show update repositories valid for update
    """

    command = "show_update_repos"

    @requires_update
    def __call__(self):
        self.display.list_update_repos(self.metadata.update_repos, self.metadata.id)
