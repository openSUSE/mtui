from logging import getLogger
import re

from ..types.rpmver import RPMVersion
from .actions import queue, spinner
from .basedoer import Doer
from .hostgroup import HostsGroup


logger = getLogger("mtui.target.downgrade")

ver_re = re.compile(r"(.*) = (.*)")


class Downgrade(Doer):
    def __init__(self, targets: HostsGroup, packages, testreport) -> None:
        super().__init__(targets, testreport)

        self.packages = packages
        self.commands: list[str] = []
        self.commands_dict: dict = {}
        self.install_command: str = ""
        self.list_command: str = ""
        self.pre_commands: list[str] = []
        self.post_commands: list[str] = []

    def run(self) -> None:
        if hasattr(self, "kind") and self.kind == "transactional":
            self._run_transactional()
        else:
            self._run()

    def _run_transactional(self):
        self.lock_hosts()
        try:
            for command in self.commands:
                self.targets.run(command)

            for t in self.targets.values():
                if "Error" in t.lasterr():
                    logger.critical(
                        '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                            t.hostname, t.lastin(), t.lastout(), t.lasterr()
                        )
                    )
                if "reboot to finish rollback" in t.lastout():
                    logger.warning(
                        "Please reboot the host {!s} to finish rollback".format(
                            t.hostname
                        )
                    )
        except BaseException:
            raise
        finally:
            self.unlock_hosts()

    def _run(self) -> None:
        versions = {}
        self.lock_hosts()
        try:
            for t in list(self.targets.values()):
                queue.put((t.set_repo, ["remove", self.testreport]))

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for t in list(self.targets.values()):
                if t.lasterr():
                    logger.critical(
                        "failed to downgrade host %s. stopping.\n# %s\n%s",
                        t.hostname,
                        t.lastin(),
                        t.lasterr(),
                    )
                    return

            self.targets.run(self.list_command)

            for hn, t in self.targets.items():
                lines: list[str] = t.lastout().split("\n")
                release: dict = {}

                for line in lines:
                    if match := re.search(ver_re, line):
                        name = match.group(1)
                        version = match.group(2)
                        release.setdefault(name, []).append(version)

                for name in release:
                    version = sorted(release[name], key=RPMVersion, reverse=True)[0]
                    versions.setdefault(hn, dict()).update({name: version})

            for command in self.pre_commands:
                self.targets.run(command)

            for package in self.packages:
                temp = self.targets.copy()
                for hn in self.targets:
                    try:
                        # self.install_command contains str with template for format ?, maybe swithch to Template type ?
                        command = self.install_command.format(
                            package, package, versions[hn][package]
                        )
                        self.commands_dict.update({hn: command})
                    except KeyError:
                        del temp[hn]
                temp.run(self.commands_dict)

                for t in self.targets.values():
                    self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())  # type: ignore

            for command in self.post_commands:
                self.targets.run(command)

        except BaseException:
            raise
        finally:
            self.unlock_hosts()
