# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2



import errno
import os

from mtui.utils import timestamp


class TargetLockedError(Exception):
    pass


class RemoteLock(object):

    """
    Localy represent the state of remote lock
    """

    def __init__(self):
        self.user = None
        """
    :param user: user owning the lock
    :type user: str or None
    """
        self.timestamp = None
        """
    :param timestamp: timestamp when the lock was set
    :type timestamp: str or None
    """
        self.pid = None
        """
    :param pid: pid of owning the lock
    :type pid: int or None
    """
        self.comment = None
        """
    :param comment: comment why the lock was set
    :type comment: str or None
    """

    def to_lockfile(self):
        """
        :return: str representation of self to be written in the
            lockfile
        """
        xs = [self.timestamp, self.user, str(self.pid)]
        if self.comment:
            xs.append(self.comment)
        return ":".join(xs)

    def __str__(self):
        if self.comment:
            comment = " ({!s})".format(self.comment)
        else:
            comment = ""

        user = self.user
        return "locked by {0}{1}.".format(user, comment)

    @classmethod
    def from_lockfile(cls, line):
        """
        :return: L{RemoteLock} instance
        """
        self = cls()

        if line == "":
            return self

        line = line.strip()
        line = line.split(":", 3)
        if len(line) < 3:
            raise ValueError('got weird format in lockfile')

        if len(line) < 4:
            line += [None]

        self.timestamp = line[0]
        self.user = line[1]
        self.pid = int(line[2])
        self.comment = line[3]

        return self


class LockedTargets(object):

    def __init__(self, targets):
        self.targets = targets

    def __enter__(self):
        for target in self.targets:
            target.lock()

    def __exit__(self, type_, value, tb):
        for target in self.targets:
            target.unlock()


class TargetLock(object):

    """
      This class is not supposted to be used directly but via
      L{Target} methods

      If the lock has comment, it is considered to be an `exclusive`
      lock. Only place that takes this into consideration is `run`
      command.
    """
    # FIXME: use netstrings to ensure proper (de)serialization
    # NOTE: the user name is not guaranteed not to collide.
    # Unfortunately, I don't see a way to do this without unreasonably
    # raising the logic complexity and usability

    filename = os.path.join('/', 'var', 'lock', 'mtui.lock')

    def __init__(self, connection, config, logger):
        self.connection = connection

        self.log = logger
        self.connection = connection

        self.i_am_user = config.session_user
        self.i_am_pid = os.getpid()
        self.timestamp_factory = timestamp
        """
    :type timestampFactory: callable
    """

        self._lock = RemoteLock()

    def load(self):
        """
        :returns None:
        """
        self.log.debug(
            '{!s}: getting mtui lock state'.format(
                self.connection.hostname))

        self._lock = RemoteLock()  # make sure lock is reset.

        try:
            lockfile = self.connection.open(self.filename)
        except EnvironmentError as error:
            if error.errno != errno.ENOENT:
                raise
            data = ""
        else:
            data = lockfile.readline()
            lockfile.close()

        self._lock = RemoteLock.from_lockfile(data)

    def is_locked(self):
        """
        :returns: bool True if target system is locked by someone else

        If possible use `try: lock.lock(); ...` as this introduces race
        condition that's fundamentally impossible to remove.
        """
        self.load()
        return bool(self._lock.user)

    def lock(self, comment=None):
        """
        Locks the target system

        :returns: None
        :raises TargetLockedError: if target is already locked.
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

        self.log.debug('{!s}: setting lock'.format(self.connection.hostname))

        rl = RemoteLock()
        rl.user = self.i_am_user
        rl.timestamp = self.timestamp_factory()
        rl.pid = self.i_am_pid
        rl.comment = comment

        try:
            lockfile = self.connection.open(self.filename, 'w+')
        except Exception as e:
            self.log.error('failed to open lockfile: {!s}'.format(e))
            raise

        lockfile.write(rl.to_lockfile())
        lockfile.close()
        self._lock = rl

    def locked_by_msg(self):
        """
        :returns str: locked by message suitable for display to user
        """
        host = self.connection.hostname
        return "{0} is {1}".format(host, str(self._lock))

    def locked_by(self):
        return self._lock

    def unlock(self, force=False):
        """
        Unlocks target system

        :param force: bool if False (default) removes only locks owned
          by current user. If True removes locks owned by anyone
          Usefull when mtui crashes (and therefore you don't own your
          locks anymore due to different pid) or someone elses mtui
          hangs and you need to access the systems
        """
        if not self.is_locked():
            return

        if not self.is_mine() and not force:
            raise TargetLockedError(self.locked_by_msg())

        try:
            self.connection.remove(self.filename)
        except IOError as e:
            if e.errno == errno.ENOENT:
                pass
        except Exception as e:
            self.log.error('failed to remove lockfile: {!s}'.format(e))
            raise

        self._lock = RemoteLock()

    def is_mine(self):
        """
        :returns bool: True if the lock is owned by user running this
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


class Locked(object):

    def __init__(
            self,
            log,
            myself,
            locked=False,
            user='nobody',
            timestamp=0,
            pid=0,
            comment=None):
        self.myself = myself
        self.log = log
        self.locked = locked
        self.user = user
        self.timestamp = timestamp
        self.pid = pid
        self.comment = comment

    def own(self):
        u = self.myself
        if not self.user == u:
            self.log.debug("user: {!s} != {!s}".format(self.user, u))
            return False

        p = str(self._getpid())
        if not self.pid == p:
            self.log.debug("pid: {!s} != {!s}".format(self.pid, p))
            return False

        return True

    def _getpid(self):
        return os.getpid()

    def time(self, style=None):
        from datetime import datetime

        if style is None:
            style = '%A, %d.%m.%Y %H:%M UTC'

        time = datetime.fromtimestamp(float(self.timestamp))

        return time.strftime(style)
