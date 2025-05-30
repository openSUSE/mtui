import os

from mtui.commands import Command


class Whoami(Command):
    """
    Display current user name and session pid.

    (username, pid) is used as user identity in rest of the codebase
    (eg. locking, logging on hosts) so it makes sense to treat this
    command consistently with those.
    """

    # TODO: consolidate these into a SessionIdentity object
    command = "whoami"

    def get_pid(self) -> int:
        return os.getpid()

    def __call__(self) -> None:
        self.println(f"User: {self.config.session_user}, app pid: {self.get_pid()}")
