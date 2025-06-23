from collections.abc import Callable
from datetime import datetime
from typing import Any, IO

from .target.hostgroup import HostsGroup
from .types import RPMVersion
from .types import System
from .utils import green, red, yellow


class CommandPromptDisplay:
    def __init__(self, output: IO) -> None:
        self.output = output

    def println(self, msg: str = "", eol: str = "\n") -> None:
        self.output.write(msg + eol)

    def list_bugs(self, bugs: dict[str, str], jira: dict[str, str], url: str) -> None:
        ids = sorted(bugs.keys())
        if ids == [""]:
            self.println("No bugs associated with Release Request.")
        else:
            self.println(f"Buglist: {url}/buglist.cgi?bug_id={','.join(ids)}")
            for bug, summary in [(bug, bugs[bug]) for bug in ids]:
                self.println()
                self.println("Bug #{0:5}: {1}".format(bug, summary))
                self.println(f"{url}/show_bug.cgi?id={bug}")

        ids = sorted(jira.keys())
        if ids == [""] or not ids:
            self.println()
            self.println("No Jira issues associated with Release Request.")
        else:
            for issue, summary in [(issue, jira[issue]) for issue in ids]:
                self.println()
                self.println("Jira #{0:5}: {1}".format(issue, summary))
                self.println(f"https://jira.suse.com/browse/{issue}")

    def list_history(self, hostname: str, system: System, lines: list[str]) -> None:
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
        if exclusive:
            mode = "serial"
        else:
            mode = "parallel"

        if state == "enabled":
            state = green("Enabled")
        elif state == "dryrun":
            state = yellow("Dryrun")
        else:
            state = red("Disabled")

        if transactional:
            trn = red("transactional")
        else:
            trn = green("standard     ")

        self.println(
            f"{hostname:<20} ({system!s:<28}): {state:<8} - {trn:<15} - ({mode})"
        )

    def list_locks(self, hostname: str, system: System, lock) -> None:
        if lock.is_locked():
            lockedby: str = "me" if lock.is_mine() else lock.locked_by()

            self.println(
                eol="",
                msg="{0:20} {1:20}: {2}".format(
                    hostname,
                    str(system),
                    yellow("since {} by {}".format(lock.time(), lockedby)),
                ),
            )

            if comment := lock.comment():
                self.println(f" : {comment}")
            else:
                self.println()
        else:
            self.println(
                "{0:20} {1:20}: {2}".format(hostname, str(system), green("not locked"))
            )

    def list_sessions(self, hostname: str, system: System, stdout: str) -> None:
        self.println(f"sessions on {hostname} ({system}):")
        self.println(stdout)

    def list_timeout(self, hostname: str, system: System, timeout: int) -> None:
        self.println(
            "{0:20} {1:20}: {2}s".format(hostname, "({!s})".format(system), timeout)
        )

    def list_versions(self, targets: HostsGroup, hosts_pvs) -> None:
        for hs, pvs in list(hosts_pvs.items()):
            if len(hosts_pvs) > 1:
                self.println("version history from:")
                for hn in hs:
                    self.println("  {} ({})".format(hn, targets[hn].system))
                self.println()

            for pkg, vers in pvs:
                self.println("{}:".format(pkg))
                indent = 0
                for ver in sorted(vers, key=RPMVersion, reverse=True):
                    self.println("  " * indent + "-> {}".format(ver))
                    indent = indent + 1
                self.println()

    def list_products(self, hostname: str, system: System) -> None:
        self.println("{}: {}".format(green("Referenece host"), yellow(hostname)))
        for x in system.pretty():
            self.println(x)
        self.println()

    def list_update_repos(self, repos, update_id) -> None:
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
        sink("log from {!s}:".format(hostname))
        for cmdline, stdout, stderr, exitcode, _ in hostlog:
            sink("{!s}:~> {!s} [{!s}]".format(hostname, cmdline, exitcode))
            sink("stdout:")
            for line in stdout.split("\n"):
                sink(line)
            sink("stderr:")
            for line in stderr.split("\n"):
                sink(line)
