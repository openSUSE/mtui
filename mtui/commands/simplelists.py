# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import requires_update


class ListBugs(Command):
    """
    Lists related bugs and corresponding Bugzilla URLs.
    """

    command = 'list_bugs'

    @requires_update
    def run(self):
        self.metadata.list_bugs(
            self.display.list_bugs,
            self.config.bugzilla_url)


class ListLocks(Command):
    """
    Lists lock state of all connected hosts
    """

    command = 'list_locks'

    def run(self):

        self.hosts.select(enabled=True).report_locks(self.display.list_locks)


class ListHosts(Command):
    """
    Lists all connected hosts including the system types and their
    current state. State could be "Enabled", "Disabled" or "Dryrun".
    """

    command = 'list_hosts'

    def run(self):

        self.targets.report_self(self.display.list_host)
