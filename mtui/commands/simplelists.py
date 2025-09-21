"""A collection of simple "list" commands."""

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices, page, requires_update


class ListBugs(Command):
    """Lists related bugs and corresponding Bugzilla URLs."""

    command = "list_bugs"

    @requires_update
    def __call__(self):
        """Executes the `list_bugs` command."""
        self.metadata.list_bugs(self.display.list_bugs, self.config.bugzilla_url)


class ListLocks(Command):
    """Lists the lock state of all connected hosts."""

    command = "list_locks"

    def __call__(self) -> None:
        """Executes the `list_locks` command."""
        self.targets.select(enabled=True).report_locks(self.display.list_locks)


class ListHosts(Command):
    """Lists all connected hosts, including their system types and state."""

    command = "list_hosts"

    def __call__(self) -> None:
        """Executes the `list_hosts` command."""
        self.targets.report_self(self.display.list_host)


class ListTimeout(Command):
    """Prints the current timeout values per host in seconds."""

    command = "list_timeout"

    def __call__(self) -> None:
        """Executes the `list_timeout` command."""
        self.targets.report_timeout(self.display.list_timeout)


class ListUpdateCommands(Command):
    """Lists all commands invoked when applying updates on target hosts."""

    command = "list_update_commands"

    def __call__(self) -> None:
        """Executes the `list_update_commands` command."""
        self.metadata.list_update_commands(self.targets, self.println)


class ListSessions(Command):
    """Lists current active SSH sessions on target hosts."""

    command = "list_sessions"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `list_sessions` command."""
        targets = self.parse_hosts()
        cmd = r"ss -r  | sed -n 's/^[^:]*:ssh *\([^ ]*\):.*/\1/p' | sort -u"

        try:
            targets.run(cmd)
        except KeyboardInterrupt:
            return

        targets.report_sessions(self.display.list_sessions)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class ListMetadata(Command):
    """Lists patchinfo metadata, such as ReviewRequestID or packager."""

    command = "list_metadata"

    @requires_update
    def __call__(self) -> None:
        """Executes the `list_metadata` command."""
        self.metadata.show_yourself(self.sys.stdout)


class ListLog(Command):
    """Prints the command protocol from the specified hosts.

    This can be useful for dumping the command history to the
    reproducer section of a template.
    """

    command = "show_log"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `show_log` command."""
        output: list[str] = []
        targets = self.parse_hosts()
        targets.report_log(self.display.show_log, output.append)
        page(output, self.prompt.interactive)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )


class ListVersions(Command):
    """Prints available package versions in enabled repositories.

    This command uses `zypper search` to find available versions and
    prints them from oldest to newest.
    """

    command = "list_versions"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-p",
            "--package",
            default=[],
            type=str,
            action="append",
            help="packagename to show versions",
        )
        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Executes the `list_versions` command."""
        targets = self.parse_hosts()
        params: list[str] = self.args.package

        self.metadata.list_versions(self.display.list_versions, targets, params)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-t", "--target"), ("-p", "--package")],
            line,
            text,
            state["hosts"].names(),
        )


class ListHistory(Command):
    """Lists a history of mtui events on the target hosts.

    This command shows the date, username, and event for each entry in
    the history. The events can be filtered by type.
    """

    command = "list_history"

    filters = {"connect", "disconnect", "install", "update", "downgrade"}

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-e",
            "--event",
            action="append",
            default=[],
            choices=cls.filters,
            help="event to list",
        )
        cls._add_hosts_arg(parser)

    def __call__(self) -> None:
        """Executes the `list_history` command."""
        targets = self.parse_hosts(enabled=False)
        option = [f"{x}" for x in set(self.args.event) & self.filters]

        count = 50
        if len(targets) >= 3:
            count = 10

        targets.report_history(self.display.list_history, count, option)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        cstring = [
            ("-t", "--target"),
            ("-e", "--event"),
            ("connect",),
            ("disconnect",),
            ("update",),
            ("downgrade",),
            ("install",),
        ]
        return complete_choices(cstring, line, text, state["hosts"].names())
