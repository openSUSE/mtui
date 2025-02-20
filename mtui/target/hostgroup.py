from collections import UserDict
from pathlib import Path
from typing import Self

from .actions import FileDelete
from .actions import FileDownload
from .actions import FileUpload
from .actions import RunCommand
from .locks import TargetLockedError
from . import Target

from mtui.messages import HostIsNotConnectedError


class HostsGroup(UserDict):
    """
    Composite pattern for Dict[hostname, Target]

    doesn't deal with Target state as that would require too much work
    to support properly. so

    1. All the given hosts are expected to be enabled.

    2. Lifetime of the object should be the same as execution of one
       command given from user (to ensure 1.)
    """

    def __init__(self, hosts: list[Target]) -> None:
        """
        :param targets: list of L{Target}
        """
        self.data: dict[str, Target] = {h.hostname: h for h in hosts}

    def select(
        self, hosts: list[str] = [], enabled: bool = False
    ) -> Self | "HostsGroup":
        if hosts == []:
            if enabled:
                return HostsGroup(
                    [h for h in self.data.values() if h.state != "disabled"]
                )
            return self

        for x in hosts:
            if x not in self.data:
                raise HostIsNotConnectedError(x)

        return HostsGroup(
            [
                h
                for hn, h in self.data.items()
                if hn in hosts and ((not enabled) or h.state != "disabled")
            ]
        )

    def unlock(self, *a, **kw) -> None:
        for x in self.data.values():
            try:
                x.unlock(*a, **kw)
            except TargetLockedError:
                pass  # logged in Target#unlock

    def lock(self, *a, **kw) -> None:
        for x in self.data.values():
            try:
                x.lock(*a, **kw)
            except TargetLockedError:
                pass

    def query_versions(self, packages):
        rs = []
        for x in self.data.values():
            rs.append((x, x.query_package_versions(packages)))

        return rs

    def add_history(self, data) -> None:
        for tgt in self.data.values():
            tgt.add_history(data)

    def names(self) -> list[str]:
        return list(self.data.keys())

    def sftp_get(self, remote: Path, local: Path) -> None:
        return FileDownload(self.data.values(), remote, local).run()

    def sftp_put(self, local: Path, remote: Path) -> None:
        return FileUpload(self.data.values(), local, remote).run()

    def sftp_remove(self, path: Path) -> None:
        return FileDelete(self.data.values(), path).run()

    def run(self, cmd) -> None:
        return self._run(cmd)

    def _run(self, cmd) -> None:
        return RunCommand(self.data, cmd).run()

    def report_self(self, sink):
        for hn in sorted(self.data.keys()):
            self.data[hn].report_self(sink)

    def report_history(self, sink, count, events) -> None:
        if events:
            self._run(
                "tac /var/log/mtui.log | grep -m {} {} | tac".format(
                    count, " ".join([('-e ":{}"'.format(e)) for e in events])
                )
            )
        else:
            self._run(f"tail -n {count} /var/log/mtui.log")

        for hn in sorted(self.data.keys()):
            self.data[hn].report_history(sink)

    def report_locks(self, sink):
        for hn in sorted(self.data.keys()):
            self.data[hn].report_locks(sink)

    def report_timeout(self, sink) -> None:
        for hn in sorted(self.data.keys()):
            self.data[hn].report_timeout(sink)

    def report_sessions(self, sink) -> None:
        for hn in sorted(self.data.keys()):
            self.data[hn].report_sessions(sink)

    def report_log(self, sink, arg) -> None:
        for hn in sorted(self.data.keys()):
            self.data[hn].report_log(sink, arg)

    def report_products(self, sink) -> None:
        for hn in sorted(self.data.keys()):
            self.data[hn].report_products(sink)
