"""Defines checks to be performed after an install action."""

from logging import getLogger
from typing import Callable

from ..exceptions import UpdateError

logger = getLogger("mtui.checks.install")


def zypper(hostname: str, stdout: str, stdin: str, stderr: str, exitcode: int) -> None:
    """Checks the output of a `zypper` command for errors.

    Args:
        hostname: The hostname where the command was run.
        stdout: The standard output of the command.
        stdin: The standard input of the command.
        stderr: The standard error of the command.
        exitcode: The exit code of the command.

    Raises:
        UpdateError: If an error is found in the output.
    """
    if exitcode in [0, 100, 101, 102, 103, 106]:
        return
    elif exitcode in (104, 4, 5, 8):
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("package not found", hostname)
    elif "A ZYpp transaction is already in progress." in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    elif "System management is locked" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    elif "Error:" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("RPM Error", hostname)
    elif "(c): c" in stdout:
        logger.critical(
            "%s: unresolved dependency problem. please resolve manually:\n%s",
            hostname,
            stdout,
        )
        raise UpdateError("Dependency Error", hostname)
    else:
        logger.critical(
            '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("Unknown Error", hostname)


#: A dictionary that maps system configurations to install check functions.
install_checks: dict[tuple[str, bool], Callable[[str, str, str, str, int], None]] = {
    ("11", False): zypper,
    ("12", False): zypper,
    ("15", False): zypper,
    ("16", False): zypper,
}
