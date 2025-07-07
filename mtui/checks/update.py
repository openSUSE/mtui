from logging import getLogger
from typing import Callable

from ..exceptions import UpdateError
from ..utils import yellow

logger = getLogger("mtui.checks.update")


def zypper(hostname: str, stdout: str, stdin: str, stderr: str, exitcode: int) -> None:
    if "zypper" in stdin and exitcode == 104:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    if "zypper" in stdin and exitcode == 106:
        logger.warning("%s: zypper returns exitcode 106:\n%s", hostname, stderr)
    if "Additional rpm output" in stdout:
        logger.warning("There was additional rpm output on %s:", hostname)
        marker = "Additional rpm output:"
        start = stdout.find(marker) + len(marker)
        end = stdout.find("Retrieving", start)
        print(stdout[start:end].replace("warning", yellow("warning")))
    if "A ZYpp transaction is already in progress." in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    if "System management is locked" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    if "(c): c" in stdout:
        logger.critical(
            "%s: unresolved dependency problem. please resolve manually:\n%s",
            hostname,
            stdout,
        )
        raise UpdateError("Dependency Error", hostname)
    if "Error:" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("RPM Error", hostname)
    if "The following package is not supported by its vendor" in stdout:
        logger.critical("%s: package support is uncertain:", hostname)
        marker = "The following package is not supported by its vendor:\n"
        start = stdout.find(marker)
        end = stdout.find("\n\n", start)
        print(stdout[start:end])


update_checks: dict[tuple[str, bool], Callable[[str, str, str, str, int], None]] = {
    ("11", False): zypper,
    ("12", False): zypper,
    ("15", False): zypper,
    ("16", False): zypper,
}
