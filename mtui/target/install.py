from logging import getLogger

from .basedoer import Doer
from .hostgroup import HostsGroup

logger = getLogger("mtui.target.install")


class Install(Doer):
    """Base install class, should not be directly used."""

    def __init__(self, targets: HostsGroup, packages: list[str]) -> None:
        self.targets = targets
        self.packages = packages
        self.commands: list[str] = []

    def run(self) -> None:
        self.lock_hosts()
        try:
            for command in self.commands:
                self.targets.run(command)

            for t in self.targets.values():
                # defined in mixin class
                self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())  # type: ignore
        except BaseException:
            raise
        finally:
            self.unlock_hosts()
