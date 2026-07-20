"""Defines checks to be performed after a downgrade action."""

from collections.abc import Callable
from logging import getLogger

from ...support.exceptions import UpdateError

logger = getLogger("mtui.checks.downgrade")


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
    # -1 is what ``Target.run`` records when the command timed out (SSH
    # no-output window exceeded) or failed to run at all. Continuing past it
    # turns an interrupted rollback into a silent half-rollback: the remaining
    # packages stay at the update version while the flow ends looking done.
    if exitcode == -1:
        logger.critical(
            '%s: command "%s" timed out or failed to run:\nstdout:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("downgrade command timed out or failed to run", hostname)
    if "A ZYpp transaction is already in progress." in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdout:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("update stack locked", hostname)
    if "System management is locked" in stderr:
        logger.critical(
            '%s: command "%s" failed:\nstdout:\n%s\nstderr:\n%s',
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
    if exitcode == 104:
        logger.critical("%s: zypper returned with errorcode 104:\n%s", hostname, stderr)
        raise UpdateError("Unspecified Error", hostname)
    if exitcode == 106:
        logger.warning("%s: zypper returned with errorcode 106:\n%s", hostname, stderr)


def transactional_update(
    hostname: str, stdout: str, stdin: str, stderr: str, exitcode: int
) -> None:
    """Checks a `transactional-update` downgrade command for a dead run.

    Only the timed-out/unrunnable gate: transactional-update's own exit
    codes and messages differ from zypper's, so the zypper-specific
    branches (104, lock strings) must not be reused here. Without this
    check the registry falls back to a no-op and a dead combined command
    sails on to the reboot with no snapshot staged.

    Raises:
        UpdateError: If the command timed out or failed to run.

    """
    if exitcode == -1:
        logger.critical(
            '%s: command "%s" timed out or failed to run:\nstdout:\n%s\nstderr:\n%s',
            hostname,
            stdin,
            stdout,
            stderr,
        )
        raise UpdateError("downgrade command timed out or failed to run", hostname)


#: A dictionary that maps system configurations to downgrade check functions.
downgrade_checks: dict[tuple[str, bool], Callable[[str, str, str, str, int], None]] = {
    ("11", False): zypper,
    ("12", False): zypper,
    ("15", False): zypper,
    ("16", False): zypper,
    ("slmicro", True): transactional_update,
}
