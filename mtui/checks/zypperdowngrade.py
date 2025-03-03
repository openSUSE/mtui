from logging import getLogger

from ..target import Target
from ..target.actions import UpdateError

logger = getLogger("mtui.checks.zypperdowngrade")


class ZypperDowngradeCheck:
    def check(
        self, target: Target, stdin: str, stdout: str, stderr: str, exitcode: int
    ) -> None: ...

    def _check(
        self, target: Target, stdin: str, stdout: str, stderr: str, exitcode: int
    ) -> None:
        if "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            )
            raise UpdateError(target.hostname, "update stack locked")
        if "System management is locked" in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdout:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
                exitcode,
            )
            raise UpdateError("update stack locked", target.hostname)
        if "(c): c" in stdout:
            logger.critical(
                "%s: unresolved dependency problem. please resolve manually:\n%s",
                target.hostname,
                stdout,
            )
            raise UpdateError("Dependency Error", target.hostname)
        if exitcode == 104:
            logger.critical(
                "%s: zypper returned with errorcode 104:\n%s", target.hostname, stderr
            )
            raise UpdateError("Unspecified Error", target.hostname)
        if exitcode == 106:
            logger.warning(
                "%s: zypper returned with errocode 106:\n%s", target.hostname, stderr
            )

        return self.check(target, stdin, stdout, stderr, exitcode)
