"""The `whoami` command."""

import os

from mtui.commands import Command


class Whoami(Command):
    """Displays the current user name and session PID.

    The username and PID are used as the user identity in the rest of
    the codebase (e.g., for locking and logging on hosts).
    """

    # TODO: consolidate these into a SessionIdentity object
    command = "whoami"

    def get_pid(self) -> int:
        """Returns the current process ID."""
        return os.getpid()

    def __call__(self) -> None:
        """Executes the `whoami` command."""
        self.println(f"User: {self.config.session_user}, app pid: {self.get_pid()}")
