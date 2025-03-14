from collections import UserDict
from logging import getLogger
from pathlib import Path
from typing import Self

from . import Target
from ..exceptions import UpdateError
from ..messages import HostIsNotConnectedError
from .actions import FileDelete
from .actions import FileDownload
from .actions import FileUpload
from .actions import RunCommand
from .actions import ThreadedMethod, queue
from .locks import TargetLockedError


logger = getLogger("mtui.target.hostgroup")


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

    def update_lock(self) -> None:
        try:
            skipped = False
            for t in self.data.values():
                if t.is_locked() and not t._lock.is_mine():
                    skipped = True
                    logger.warning(
                        "host %s is locked since %s by %s. skipping.",
                        t.hostname,
                        t._lock.time(),
                        t._lock.locked_by(),
                    )
                    if t._lock.comment():
                        logger.info(
                            "%s's comment: %s", t._lock.locked_by(), t._lock.comment()
                        )
                else:
                    t.lock()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for t in self.data.values():
                    try:
                        t.unlock()
                    except AssertionError:
                        pass
                raise UpdateError("Hosts locked")
        except BaseException:
            raise

    def perform_install(self, packages) -> None:
        commands = {
            t.hostname: t.get_installer()["command"].substitute(
                packages=" ".join(packages)
            )
            for t in self.data.values()
        }
        self.update_lock()
        try:
            self.run(commands)
            for t in self.data.values():
                t.get_installer_check()(
                    t.hostname, t.lastout(), t.lastin(), t.lasterr(), t.lastexit()
                )
        except BaseException:
            raise
        finally:
            self.unlock()

    def perform_uninstall(self, packages) -> None:
        commands = {
            t.hostname: t.get_uninstaller()["command"].substitute(
                packages=" ".join(packages)
            )
            for t in self.data.values()
        }
        self.update_lock()
        try:
            self.run(commands)
            for t in self.data.values():
                t.get_uninstaller_check()(
                    t.hostname, t.lastout(), t.lastin(), t.lasterr(), t.lastexit()
                )
        except BaseException:
            raise
        finally:
            self.unlock()

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
