from logging import getLogger

from ..exceptions import UpdateError
from ..target import Target

logger = getLogger("mtui.checks.zypperprepare")


class ZypperPrepareCheck:
    def check(
        self, target: Target, stdin: str, stdout: str, stderr: str, exitcode: int
    ) -> None: ...

    def _check(
        self, target: Target, stdin: str, stdout: str, stderr: str, exitcode: int
    ) -> None:
        if "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError(target.hostname, "update stack locked")
        if "System management is locked" in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("update stack locked", target.hostname)
        if "(c): c" in stdout:
            logger.critical(
                "%s: unresolved dependency problem. please resolve manually:\n%s",
                target.hostname,
                stdout,
            )
            raise UpdateError("Dependency Error", target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)
