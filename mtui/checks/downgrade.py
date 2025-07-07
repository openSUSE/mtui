from logging import getLogger
from typing import Callable

from ..exceptions import UpdateError

logger = getLogger("mtui.checks.downgrade")


def zypper(hostname: str, stdout: str, stdin: str, stderr: str, exitcode: int) -> None:
    if "A ZYpp transaction is already in progress." in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
        )
        raise UpdateError(hostname, "update stack locked")
    if "System management is locked" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdout:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
            exitcode,
        )
        raise UpdateError("update stack locked", hostname)
    if "(c): c" in stdout:
        logger.critical(
            "%s: unresolved dependency problem. please resolve manually:\n%s",
            hostname,
            stdout,
        )
        raise UpdateError("Dependency Error", hostname)
    if exitcode == 104:
        logger.critical("%s: zypper returned with errorcode 104:\n%s", hostname, stderr)
        raise UpdateError("Unspecified Error", hostname)
    if exitcode == 106:
        logger.warning("%s: zypper returned with errocode 106:\n%s", hostname, stderr)


downgrade_checks: dict[tuple[str, bool], Callable[[str, str, str, str, int], None]] = {
    ("11", False): zypper,
    ("12", False): zypper,
    ("15", False): zypper,
    ("16", False): zypper,
}
