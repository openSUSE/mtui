from logging import getLogger

from .actions import UpdateError
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
                self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())
        except BaseException:
            raise
        finally:
            self.unlock_hosts()

    def _check(self, target, stdin, stdout, stderr, exitcode) -> None:
        if exitcode in [0, 100, 101, 102, 103, 106]:
            return self.check(target, stdin, stdout, stderr, exitcode)
        if "zypper" in stdin and exitcode == 104:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("package not found", target.hostname)
        elif "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("update stack locked", target.hostname)
        elif "System management is locked" in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("update stack locked", target.hostname)
        elif "Error:" in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("RPM Error", target.hostname)
        elif "(c): c" in stdout:
            logger.critical(
                "%s: unresolved dependency problem. please resolve manually:\n%s",
                target.hostname,
                stdout,
            )
            raise UpdateError("Dependency Error", target.hostname)
        else:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("Unknown Error", target.hostname)
