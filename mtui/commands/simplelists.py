# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import requires_update
from mtui.utils import complete_choices


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


class ListTimeout(Command):
    """
    Prints the current timeout values per host in seconds.
    """
    command = 'list_timeout'

    def run(self):

        self.targets.report_timeout(self.display.list_timeout)


class ListUpdateCommands(Command):
    """
    List all commands which are invoked when applying updates on the
    target hosts.
    """
    command = 'list_update_commands'

    def run(self):
        self.metadata.list_update_commands(self.targets, self.println)

class ListSessions(Command):
    """
    Lists current active ssh sessions on target hosts.
    """
    command = 'list_sessions'

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        targets = self.parse_hosts(self.args.hosts)
        cmd = "ss -r  | sed -n 's/^[^:]*:ssh *\([^ ]*\):.*/\\1/p' | sort -u"

        try:
            targets.run(cmd)
        except KeyboardInterrupt:
            return

        targets.report_sessions(self.display.list_sessions)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices([('-t', '--target'), ],
                                line, text, state['hosts'].names())





