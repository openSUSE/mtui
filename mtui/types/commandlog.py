"""A named tuple for representing a command log entry."""

from typing import NamedTuple


class CommandLog(NamedTuple):
    """A named tuple that represents a log entry for a command.

    Attributes:
        command: The command that was run.
        stdout: The standard output of the command.
        stderr: The standard error of the command.
        exitcode: The exit code of the command.
        runtime: The runtime of the command in seconds.
    """

    command: str
    stdout: str
    stderr: str
    exitcode: int
    runtime: int
