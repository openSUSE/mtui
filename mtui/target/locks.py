"""Classes for managing locks on target hosts."""

from datetime import datetime
import errno
from logging import getLogger
import os
from pathlib import Path
from typing import Self

from ..config import Config
from ..connection import Connection
from ..utils import timestamp

logger = getLogger("mtui.target.locks")


class TargetLockedError(Exception):
    """Exception raised when a target is locked."""

    pass


class RemoteLock:
    """Represents the state of a remote lock."""

    def __init__(self) -> None:
        """Initializes the `RemoteLock` object."""
        self.user: str = ""
        self.timestamp: str = ""
        self.pid: int = 0
        self.comment: str = ""

    def to_lockfile(self) -> str:
        """Returns a string representation of the lock for a lockfile.

        Returns:
            A string representation of the lock.
        """
        xs: list[str] = [self.timestamp, self.user, str(self.pid)]
        if self.comment:
            xs.append(self.comment)
        return ":".join(xs)

    def __str__(self) -> str:
        """Returns a human-readable string representation of the lock."""
        if self.comment:
            comment = f" ({self.comment})"
        else:
            comment = ""

        return f"locked by {self.user}{comment}."

    @classmethod
    def from_lockfile(cls, line) -> Self:
        """Creates a `RemoteLock` instance from a lockfile line.

        Args:
            line: The line from the lockfile.

        Returns:
            A `RemoteLock` instance.
        """
        self = cls()

        if line == "":
            return self

        line = line.strip()
        line = line.split(":", 3)
        if len(line) < 3:
            raise ValueError("got weird format in lockfile")

        if len(line) < 4:
            line += [""]

        self.timestamp = line[0]
        self.user = line[1]
        self.pid = int(line[2])
        self.comment = line[3]

        return self


class LockedTargets:
    """A context manager for locking a group of targets."""

    def __init__(self, targets) -> None:
        """Initializes the `LockedTargets` context manager.

        Args:
            targets: A list of targets to lock.
        """
        self.targets = targets

    def __enter__(self) -> None:
        """Locks all targets in the group."""
        for target in self.targets:
            target.lock()

    def __exit__(self, type_, value, tb) -> None:
        """Unlocks all targets in the group."""
        for target in self.targets:
            target.unlock()


class TargetLock:
    """Manages the lock for a single target.

    This class is not intended to be used directly, but rather via
    the methods of the `Target` class.

    If the lock has a comment, it is considered to be an `exclusive`
    lock. This is only taken into consideration by the `run` command.
    """

    # FIXME: use netstrings to ensure proper (de)serialization
    # NOTE: the user name is not guaranteed not to collide.
    # Unfortunately, I don't see a way to do this without unreasonably
    # raising the logic complexity and usability

    filename = Path("/var/lock/mtui.lock")

    def __init__(self, connection: Connection, config: Config) -> None:
        """Initializes the `TargetLock` object.

        Args:
            connection: The connection to the target host.
            config: The application configuration.
        """
        self.connection = connection
        self.i_am_user = config.session_user  # type: ignore
        self.i_am_pid = os.getpid()
        """
    :type timestampFactory: callable
    """

        self._lock = RemoteLock()

    # TODO: some cache needed
    def load(self) -> None:
        """Loads the lock state from the remote host."""
        logger.debug(f"{self.connection.hostname}: getting mtui lock state")

        self._lock = RemoteLock()  # make sure lock is reset.

        try:
            lockfile = self.connection.sftp_open(self.filename)
        except OSError as error:
            if error.errno != errno.ENOENT:
                raise
            data = ""
        else:
            data = lockfile.readline()
            lockfile.close()

        self._lock = RemoteLock.from_lockfile(data)

    def is_locked(self) -> bool:
        """Checks if the target system is locked by someone else.

        Returns:
            True if the target is locked, False otherwise.
        """
        self.load()
        return bool(self._lock.user)

    def lock(self, comment: str = "") -> None:
        """Locks the target system.

        Args:
            comment: An optional comment for the lock.

        Raises:
            TargetLockedError: If the target is already locked.
        """
        if self.is_locked():
            # NOTE: there is a slight race between between getting the
            # state of the lock on target host and setting the lock.
            # However, that has always been here afaik.
            # TODO: test if using sftpclient.mkdir can be used to make
            # the locking really atomic.
            if not self.is_mine():
                # NOTE: let the code pass through if is_mine() as
                # setting a different comment may be desired.
                raise TargetLockedError(self.locked_by_msg())

        logger.debug("%s: setting lock", self.connection.hostname)

        rl = RemoteLock()
        rl.user = self.i_am_user
        rl.timestamp = timestamp()
        rl.pid = self.i_am_pid
        rl.comment = comment

        try:
            lockfile = self.connection.sftp_open(self.filename, "w+")
            lockfile.write(rl.to_lockfile())
            lockfile.close()
        except Exception as e:
            logger.error("failed to open lockfile: %s", e)
            raise

        self._lock = rl

    def locked_by_msg(self) -> str:
        """Returns a "locked by" message suitable for display to the user.

        Returns:
            A "locked by" message.
        """
        self.load()
        return f"{self.connection.hostname} is {self._lock}"

    def locked_by(self) -> str:
        """Returns the user who locked the target.

        Returns:
            The user who locked the target.
        """
        self.load()
        return self._lock.user

    def comment(self) -> str:
        """Returns the comment for the lock.

        Returns:
            The comment for the lock.
        """
        self.load()
        return self._lock.comment

    def time(self) -> str:
        """Returns the time the lock was created.

        Returns:
            The time the lock was created.
        """
        self.load()
        time = datetime.fromtimestamp(float(self._lock.timestamp))
        return time.strftime(r"%A, %d.%m.%Y %H:%M UTC")

    def unlock(self, force: bool = False) -> None:
        """Unlocks the target system.

        Args:
            force: If True, removes locks owned by anyone.
        """
        if not self.is_locked():
            return

        if not self.is_mine() and not force:
            raise TargetLockedError(self.locked_by_msg())

        try:
            self.connection.sftp_remove(self.filename)
        except IOError as e:
            if e.errno == errno.ENOENT:
                pass
        except Exception as e:
            logger.error("failed to remove lockfile: %s", e)
            raise

        self._lock = RemoteLock()

    def is_mine(self) -> bool:
        """Checks if the lock is owned by the current user.

        Returns:
            True if the lock is owned by the current user, False otherwise.
        """
        if not self._lock.user:
            raise RuntimeError("not locked")

        if self._lock.user != self.i_am_user:
            return False
        if self._lock.pid != self.i_am_pid:
            # NOTE: checking pid handles the case where one user is
            # running multiple mtui instances against the same hosts
            return False
        return True
