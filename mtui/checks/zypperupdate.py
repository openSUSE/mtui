from logging import getLogger

from ..exceptions import UpdateError
from ..target import Target
from ..utils import yellow


logger = getLogger("mtui.checks.zypperupdate")


class ZypperUpdateCheck:
    def _check(self, target: Target, stdin, stdout, stderr, exitcode: int) -> None:
        if "zypper" in stdin and exitcode == 104:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("update stack locked", target.hostname)
        if "zypper" in stdin and exitcode == 106:
            logger.warning(
                "%s: zypper returns exitcode 106:\n%s", target.hostname, stderr
            )
        if "Additional rpm output" in stdout:
            logger.warning("There was additional rpm output on %s:", target.hostname)
            marker = "Additional rpm output:"
            start = stdout.find(marker) + len(marker)
            end = stdout.find("Retrieving", start)
            print(stdout[start:end].replace("warning", yellow("warning")))
        if "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("update stack locked", target.hostname)
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
