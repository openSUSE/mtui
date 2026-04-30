"""Handles the display of formatted output in the command prompt."""

from collections.abc import Callable
from datetime import datetime
from typing import IO, Any

from .target.hostgroup import HostsGroup
from .types import RPMVersion, System
from .utils import green, red, yellow


class CommandPromptDisplay:
    """Handles the display of formatted output in the command prompt."""

    def __init__(self, output: IO) -> None:
        """Initializes the display object.

        Args:
            output: The output stream to write to.

        """
        self.output = output

    def println(self, msg: str = "", eol: str = "\n") -> None:
        """Prints a message to the output stream.

        Args:
            msg: The message to print.
            eol: The end-of-line character to use.

        """
        self.output.write(msg + eol)

    def list_bugs(self, bugs: dict[str, str], jira: dict[str, str], url: str) -> None:
        """Displays a list of bugs and Jira issues.

        Args:
            bugs: A dictionary of bug IDs and summaries.
            jira: A dictionary of Jira issue IDs and summaries.
            url: The base URL for the bug tracker.

        """
        ids = sorted(bugs.keys())
        if ids == [""]:
            self.println("No bugs associated with Release Request.")
        else:
            self.println(f"Buglist: {url}/buglist.cgi?bug_id={','.join(ids)}")
            for bug, summary in [(bug, bugs[bug]) for bug in ids]:
                self.println()
                self.println(f"Bug #{bug:5}: {summary}")
                self.println(f"{url}/show_bug.cgi?id={bug}")

        ids = sorted(jira.keys())
        if ids == [""] or not ids:
            self.println()
            self.println("No Jira issues associated with Release Request.")
        else:
            for issue, summary in [(issue, jira[issue]) for issue in ids]:
                self.println()
                self.println(f"Jira #{issue:5}: {summary}")
                self.println(f"https://jira.suse.com/browse/{issue}")

    def list_history(self, hostname: str, system: System, lines: list[str]) -> None:
        """Displays the command history for a host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.
            lines: A list of history log lines.

        """
        self.println(f"history from {hostname} ({system}):")
        lines.reverse()
        for line in lines:
            try:
                when = line.split(":")[0]
                who = line.split(":")[1]
                event = ":".join(line.split(":")[2:])
            except IndexError:
                continue

            time = datetime.fromtimestamp(float(when))
            self.println(
                "{}, {}: {}".format(time.strftime("%A, %d.%m.%Y %H:%M"), who, event)
            )
        self.println()

    def list_host(
        self,
        hostname: str,
        system: System,
        transactional: bool,
        state: str,
        exclusive: str,
    ) -> None:
        """Displays the status of a host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.
            transactional: Whether the host is transactional.
            state: The state of the host (enabled, disabled, or dryrun).
            exclusive: Whether the host is in exclusive mode.

        """
        mode = "serial" if exclusive else "parallel"

        if state == "enabled":
            state = green("Enabled")
        elif state == "dryrun":
            state = yellow("Dryrun")
        else:
            state = red("Disabled")

        trn = red("transactional") if transactional else green("standard     ")

        self.println(
            f"{hostname:<20} ({system!s:<28}): {state:<8} - {trn:<15} - ({mode})"
        )

    def list_locks(self, hostname: str, system: System, lock) -> None:
        """Displays the lock status of a host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.
            lock: The lock object for the host.

        """
        if lock.is_locked():
            lockedby: str = "me" if lock.is_mine() else lock.locked_by()

            self.println(
                eol="",
                msg="{:20} {:20}: {}".format(
                    hostname,
                    str(system),
                    yellow(f"since {lock.time()} by {lockedby}"),
                ),
            )

            if comment := lock.comment():
                self.println(f" : {comment}")
            else:
                self.println()
        else:
            self.println(
                "{:20} {:20}: {}".format(hostname, str(system), green("not locked"))
            )

    def list_sessions(self, hostname: str, system: System, stdout: str) -> None:
        """Displays the active sessions on a host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.
            stdout: The output of the session listing command.

        """
        self.println(f"sessions on {hostname} ({system}):")
        self.println(stdout)

    def list_timeout(self, hostname: str, system: System, timeout: int) -> None:
        """Displays the command timeout for a host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.
            timeout: The command timeout in seconds.

        """
        self.println("{:20} {:20}: {}s".format(hostname, f"({system!s})", timeout))

    def list_versions(self, targets: HostsGroup, hosts_pvs) -> None:
        """Displays the version history of packages on a host.

        Args:
            targets: The group of target hosts.
            hosts_pvs: A dictionary mapping hosts to package versions.

        """
        for hs, pvs in list(hosts_pvs.items()):
            if len(hosts_pvs) > 1:
                self.println("version history from:")
                for hn in hs:
                    self.println(f"  {hn} ({targets[hn].system})")
                self.println()

            for pkg, vers in pvs:
                self.println(f"{pkg}:")
                indent = 0
                for ver in sorted(vers, key=RPMVersion, reverse=True):
                    self.println("  " * indent + f"-> {ver}")
                    indent = indent + 1
                self.println()

    def list_products(self, hostname: str, system: System) -> None:
        """Displays the products of a reference host.

        Args:
            hostname: The name of the host.
            system: The system information for the host.

        """
        self.println("{}: {}".format(green("Referenece host"), yellow(hostname)))
        for x in system.pretty():
            self.println(x)
        self.println()

    def list_update_repos(self, repos) -> None:
        """Displays the update repositories.

        Args:
            repos: A dictionary of repositories.

        """
        for p, r in repos.items():
            self.println(
                "{}: {} - {}: {} - {}: {}".format(
                    green("Product"),
                    yellow(p.name),
                    green("version"),
                    yellow(p.version),
                    green("arch"),
                    yellow(p.arch),
                )
            )
            self.println(f"    {r}")

    @staticmethod
    def show_log(
        hostname: str, hostlog: list[tuple[str, str, str, int, Any]], sink: Callable
    ) -> None:
        """Displays the command log for a host.

        Args:
            hostname: The name of the host.
            hostlog: A list of log entries.
            sink: The function to use for printing the log.

        """
        sink(f"log from {hostname!s}:")
        for cmdline, stdout, stderr, exitcode, _ in hostlog:
            sink(f"{hostname!s}:~> {cmdline!s} [{exitcode!s}]")
            sink("stdout:")
            for line in stdout.split("\n"):
                sink(line)
            sink("stderr:")
            for line in stderr.split("\n"):
                sink(line)
