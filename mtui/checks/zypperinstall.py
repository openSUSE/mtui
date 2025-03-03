from logging import getLogger

from ..target import Target
from ..target.actions import UpdateError

logger = getLogger("mtui.checks.zypperinstall")


class ZypperInstallCheck:
    def check(self, target: Target, stdin, stdout, stderr, exitcode: int) -> None:
        pass

    def _check(self, target: Target, stdin, stdout, stderr, exitcode: int) -> None:
        if exitcode in [0, 100, 101, 102, 103, 106]:
            return self.check(target, stdin, stdout, stderr, exitcode)
        if exitcode in (104, 4, 5, 8):
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
