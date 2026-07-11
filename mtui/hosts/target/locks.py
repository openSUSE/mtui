"""Classes for managing locks on target hosts."""

import errno
import os
import time
from datetime import UTC, datetime
from logging import getLogger
from pathlib import Path
from traceback import format_exc
from typing import Self

from ...support.config import Config
from ...support.fileops import timestamp
from ..connection import Connection

logger = getLogger("mtui.target.locks")


class TargetLockedError(Exception):
    """Exception raised when a target is locked."""


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
        comment = f" ({self.comment})" if self.comment else ""

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
        self.config = config
        self.i_am_user = config.session_user
        self.i_am_pid = os.getpid()
        """
    :type timestampFactory: callable
    """

        self._lock = RemoteLock()

    # TODO: some cache needed
    def load(self) -> None:
        """Loads the lock state from the remote host."""
        logger.debug("%s: getting mtui lock state", self.connection.hostname)

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

    def age_seconds(self) -> int | None:
        """Returns the age of the current remote lock in seconds.

        Returns:
            The lock age in seconds, or None when there is no lock or the
            stored timestamp is missing/malformed (so callers treat such a
            lock as "leave it alone").

        """
        self.load()
        if not self._lock.user or not self._lock.timestamp:
            return None
        try:
            return int(time.time()) - int(self._lock.timestamp)
        except ValueError:
            logger.debug(
                "%s: malformed lock timestamp %r",
                self.connection.hostname,
                self._lock.timestamp,
            )
            return None

    def reap_if_stale(self) -> bool:
        """Force-removes the remote lock if it is older than the threshold.

        Controlled by the ``lock_reap_stale`` (default on) and
        ``lock_stale_age`` (seconds, default 86400) config options. Applies
        to every lock, including exclusive (commented) ones and locks owned
        by other users, since a sufficiently old lock is almost always left
        over from a crashed or abandoned session.

        Returns:
            True if a stale lock was removed, False otherwise.

        """
        if not self.config.lock_reap_stale or self.config.lock_stale_age <= 0:
            return False

        age = self.age_seconds()
        if age is None or age <= self.config.lock_stale_age:
            return False

        logger.warning(
            "%s: removing stale lock held by %s (%d h old)",
            self.connection.hostname,
            self._lock.user,
            age // 3600,
        )
        self.unlock(force=True)
        return True

    def try_claim(self, comment: str = "") -> bool:
        """Claim the remote lock without raising when it is busy.

        Unlike :meth:`lock`, a host already locked by someone else does not
        raise — it returns ``False`` so a pool caller can move on to the next
        candidate. The claim succeeds when the host is free, already ours, or
        the existing lock is stale enough to reap.

        Args:
            comment: Optional comment recorded with the lock (e.g. the pool
                owner stamp ``mtui pool <RRID> [<owner>]``).

        Returns:
            ``True`` if the lock is now ours, ``False`` if it is held by
            someone else.

        """
        if self.is_locked() and not self.is_mine() and not self.reap_if_stale():
            return False
        try:
            self.lock(comment)
        except TargetLockedError:
            return False
        return True

    def _wait_for_lock(self) -> bool:
        """Queue for a busy remote lock up to ``[lock] wait`` seconds.

        Polls the remote lock every ``[lock] wait_poll`` seconds until it is
        gone, has become ours, or is reaped as stale. Logs a warning on
        wait-start and again on timeout. With ``[lock] wait <= 0`` (default)
        this is a no-op returning ``False`` immediately, preserving the
        historical fail-fast behaviour (RFC §5.8).

        Returns:
            ``True`` if the lock became free/ours/reaped within the budget
            (the caller may proceed to claim it), ``False`` if it is still
            held by someone else after the wait.

        """
        wait = self.config.lock_wait
        if wait <= 0:
            return False
        poll = max(1, self.config.lock_wait_poll)
        deadline = time.monotonic() + wait
        logger.warning(
            "%s: locked by %s; waiting up to %ds for it to free",
            self.connection.hostname,
            self._lock.user,
            wait,
        )
        while True:
            if self.reap_if_stale():
                return True
            time.sleep(min(poll, max(0, deadline - time.monotonic())))
            if not self.is_locked() or self.is_mine():
                return True
            if time.monotonic() >= deadline:
                logger.warning(
                    "%s: still locked by %s after %ds; giving up",
                    self.connection.hostname,
                    self._lock.user,
                    wait,
                )
                return False

    def lock(self, comment: str = "") -> None:
        """Locks the target system.

        Args:
            comment: An optional comment for the lock.

        Raises:
            TargetLockedError: If the target is already locked.

        """
        logger.debug("%s: setting lock", self.connection.hostname)

        rl = RemoteLock()
        rl.user = self.i_am_user
        rl.timestamp = timestamp()
        rl.pid = self.i_am_pid
        rl.comment = comment

        # First try an *atomic exclusive create* (paramiko maps mode ``"x"`` to
        # ``O_CREAT | O_EXCL``): on a free host exactly one of two racing
        # processes wins the create and the loser falls through to the
        # reconciliation below. This closes the read-then-write TOCTOU the
        # previous "stat, then ``w+``" sequence had.
        if self._write_lockfile(rl, exclusive=True):
            self._lock = rl
            return

        # The file already exists. Decide whether we may overwrite it: our own
        # lock (re-stamp with a possibly-new comment), a stale lock we may reap,
        # or one that frees within the optional wait budget. Otherwise refuse.
        if self.is_locked() and not self.is_mine() and not self._wait_for_lock():
            raise TargetLockedError(self.locked_by_msg())

        if not self._write_lockfile(rl, exclusive=False):
            logger.error("failed to open lockfile")
            raise OSError(f"failed to write lockfile on {self.connection.hostname}")

        self._lock = rl

    def _write_lockfile(self, rl: RemoteLock, *, exclusive: bool) -> bool:
        """Write ``rl`` to the remote lockfile; return whether it succeeded.

        With ``exclusive=True`` the file is opened ``"x"`` (atomic create that
        fails if it already exists) and a "file exists" failure returns
        ``False`` so the caller can reconcile. With ``exclusive=False`` the file
        is opened ``"w+"`` (truncate/overwrite) and any failure returns
        ``False``. Both modes log unexpected errors at DEBUG for diagnosis.
        """
        mode = "x" if exclusive else "w+"
        try:
            lockfile = self.connection.sftp_open(self.filename, mode)
            lockfile.write(rl.to_lockfile())
            lockfile.close()
        except Exception:  # noqa: BLE001 - all failures are non-fatal here
            # An exclusive create racing a winner (file already exists) is
            # expected; any other failure is logged for diagnosis. Either way
            # the caller decides what to do next.
            logger.debug(format_exc())
            return False
        return True

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
        """Returns the time the lock was created, rendered in UTC.

        The epoch timestamp is shared between testers in different
        timezones, so it is converted timezone-aware to match the
        literal "UTC" label in the output.

        Returns:
            The time the lock was created, or ``"unknown"`` when the stored
            timestamp is missing or malformed. This mirrors ``age_seconds``:
            a bad timestamp must not raise (this is called from
            ``update_lock`` while reporting a foreign lock, where an exception
            would abort the whole lock-acquisition walk).

        """
        self.load()
        try:
            ts = datetime.fromtimestamp(float(self._lock.timestamp), tz=UTC)
        except (ValueError, OverflowError, OSError):
            logger.debug(
                "%s: malformed lock timestamp %r",
                self.connection.hostname,
                self._lock.timestamp,
            )
            return "unknown"
        return ts.strftime(r"%A, %d.%m.%Y %H:%M UTC")

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
        except OSError as e:
            if e.errno == errno.ENOENT:
                logger.debug("lockfile %s already gone", self.filename)
            else:
                logger.debug(
                    "ignoring OSError while removing lockfile %s",
                    self.filename,
                    exc_info=True,
                )
        except Exception:
            logger.error("failed to remove lockfile")
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
        # NOTE: checking pid handles the case where one user is
        # running multiple mtui instances against the same hosts
        return self._lock.pid == self.i_am_pid


class PoolLock(TargetLock):
    """Remote lock for refhost *pool* claims, separate from the zypper lock.

    A pool claim marks a reference host as taken by a particular template
    (RRID) so a different user/template does not connect to the same host,
    while the host-arbitration pool selection runs. It lives in its own
    remote file (:attr:`filename`) so it never collides with the operation
    lock taken around zypper transactions (``lock``/``unlock``,
    ``update_lock``, ``LockedTargets``) -- the two mechanisms are fully
    independent.

    Ownership differs from :class:`TargetLock` deliberately: a pool lock is
    "mine" when it was taken by the **same template (RRID) and user**,
    regardless of PID. The pool lock outlives the process that took it (a
    tester may reconnect from a fresh ``mtui`` invocation), so a PID-based
    identity -- as the base class uses for the per-process operation lock --
    would wrongly block a session from reconnecting to a host it already
    claimed.

    The RRID is stored in the lock comment as ``mtui pool <RRID> [<owner>]``
    (see :meth:`mtui.test_reports.testreport.TestReport.connect_target`).
    """

    filename = Path("/var/lock/mtui-pool.lock")

    def __init__(self, connection: Connection, config: Config, rrid: str = "") -> None:
        """Initializes the `PoolLock` object.

        Args:
            connection: The connection to the target host.
            config: The application configuration.
            rrid: The RRID of the owning template. When empty (e.g. a
                directly-constructed report that never uses pool selection),
                ownership degrades to user-only.

        """
        super().__init__(connection, config)
        self.i_am_rrid = rrid

    @staticmethod
    def _rrid_of(comment: str) -> str:
        """Extracts the RRID from a ``mtui pool <RRID> [<owner>]`` comment.

        Args:
            comment: The lock comment.

        Returns:
            The RRID, or ``""`` when the comment is not a pool comment.

        """
        parts = comment.split()
        if len(parts) >= 3 and parts[0] == "mtui" and parts[1] == "pool":
            return parts[2]
        return ""

    def is_mine(self) -> bool:
        """Checks if the pool lock is owned by this template + user.

        Unlike :meth:`TargetLock.is_mine`, ownership ignores the PID and
        compares the RRID recorded in the lock comment against this
        session's RRID, so a tester reconnecting from a fresh process is
        recognized as the owner.

        Returns:
            True if the lock belongs to this template and user, False
            otherwise.

        """
        if not self._lock.user:
            raise RuntimeError("not locked")

        if self._lock.user != self.i_am_user:
            return False
        # Pool locks outlive the process, so identity is RRID-based, not PID.
        # When this session has no RRID (non-pool report), fall back to
        # user-only ownership.
        if not self.i_am_rrid:
            return True
        return self._rrid_of(self._lock.comment) == self.i_am_rrid
