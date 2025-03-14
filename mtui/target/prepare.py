from logging import getLogger

from .actions import queue, spinner
from .basedoer import Doer
from .hostgroup import HostsGroup

logger = getLogger("mtui.target.prepare")


class Prepare(Doer):
    def __init__(
        self,
        targets: HostsGroup,
        packages,
        testreport,
        testing: bool = False,
        force: bool = False,
        installed_only: bool = False,
    ) -> None:
        super().__init__(targets, testreport)
        self.packages = packages
        self.testing = testing
        self.force = force
        self.installed_only = installed_only
        self.commands: list[str] = []

    def run(self) -> None:
        self.lock_hosts()

        try:
            for t in self.targets.values():
                if self.testing:
                    queue.put((t.set_repo, ["add", self.testreport]))
                else:
                    queue.put((t.set_repo, ["remove", self.testreport]))

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for t in self.targets.values():
                if t.lasterr():
                    logger.critical(
                        "failed to prepare host %s. stopping.\n# %s\n%s",
                        t.hostname,
                        t.lastin(),
                        t.lasterr(),
                    )
                    return

            for command in self.commands:
                self.targets.run(command)

                for t in self.targets.values():
                    self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())  # type: ignore
        except BaseException:
            raise
        finally:
            self.unlock_hosts()
