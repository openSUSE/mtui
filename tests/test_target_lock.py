from nose.tools import ok_, eq_, raises

from mtui.target import TargetLock, RemoteLock, TargetLockedError

from .utils import ConfigFake

import errno

@raises(IOError)
def test_error_opening_lockfile_other_than_missing_raises():
    class ConnMock(object):
        hostname = 'bar'

        def open(self, fn):
            raise IOError()

    l = TargetLock(ConnMock(), ConfigFake(dict(session_user = 'foo')))
    l.load()

def test_not_locked_on_missing_lockfile():
    class ConnMock(object):
        hostname = 'bar'

        def open(self, fn):
            e = IOError()
            e.errno = errno.ENOENT
            raise e

    l = TargetLock(ConnMock(), ConfigFake(dict(session_user = 'foo')))
    l.load()

def test_not_locked_on_empty_lockfile():
    class Lockfile(object):
        closed = False

        def readline(self):
            return ''

        def close(self):
            self.closed = True

    class ConnMock(object):
        hostname = 'bar'
        lockfile = Lockfile()

        def open(self, fn):
            return self.lockfile


    conn = ConnMock()

    l = TargetLock(conn, ConfigFake(dict(session_user = 'foo')))
    eq_(l.is_locked(), False)
    ok_(conn.lockfile.closed)

def test_parse_lockfile():
    l = RemoteLock.from_lockfile('')
    eq_(l.timestamp, None)
    eq_(l.user, None)
    eq_(l.pid, None)
    eq_(l.comment, None)

    l = RemoteLock.from_lockfile('00-00:foo:666')
    eq_(l.timestamp, '00-00')
    eq_(l.user, 'foo')
    eq_(l.pid, 666)
    eq_(l.comment, None)

    l = RemoteLock.from_lockfile('00-00:foo:666:bar')
    eq_(l.timestamp, '00-00')
    eq_(l.user, 'foo')
    eq_(l.pid, 666)
    eq_(l.comment, 'bar')

    l2 = RemoteLock.from_lockfile("")
    eq_(l2.timestamp, None)
    eq_(l2.user, None)
    eq_(l2.pid, None)
    eq_(l2.comment, None)

def test_lock_locks():
    class Lockfile(object):
        closed = False
        written = ""

        def readline(self):
            return ''

        def close(self):
            self.closed = True

        def write(self, x):
            self.written += x

    class ConnMock(object):
        hostname = 'bar'
        lockfile = Lockfile()

        def open(self, fn, mode):
            return self.lockfile

    conn = ConnMock()

    l = TargetLock(conn, ConfigFake(dict(session_user = 'foo')))
    l.is_locked = lambda: False
    l.timestamp_factory = lambda: "00-00"
    l.i_am_pid = 666
    l.lock("kek")

    eq_(conn.lockfile.written, "00-00:foo:666:kek")

    rl = l.locked_by()
    eq_(rl.user, 'foo')
    eq_(rl.pid, 666)
    eq_(rl.comment, 'kek')
    eq_(rl.timestamp, '00-00')

def test_unlock_doesnt_unlock_unlocked():
    class ConnMock(object):
        def remove(self, fn):
            ok_(False)

    l = TargetLock(ConnMock(), ConfigFake(dict(session_user = 'foo')))
    l.i_am_pid = 666

    rl = RemoteLock()

    l.load = lambda: None
    l.unlock()

def test_lock_is_mine():
    l = TargetLock(None, ConfigFake(dict(session_user = 'foo')))
    l.i_am_pid = 666

    rl = RemoteLock()
    rl.user = 'foo'
    rl.pid = 666

    l._lock = rl
    eq_(l.is_mine(), True)


class ConnRemovingMock(object):
    hostname = 'bar'

    def __init__(self):
        self.removed = []

    def remove(self, fn):
        self.removed.append(fn)

def test_lock_unlocks():
    conn = ConnRemovingMock()

    l = TargetLock(conn, ConfigFake(dict(session_user = 'foo')))
    l.i_am_pid = 666
    l.load = lambda: None

    rl = RemoteLock()
    rl.user = 'foo'
    rl.pid = 666

    l._lock = rl
    l.unlock()
    _unlock_post_cond(l, conn)

def _unlock_post_cond(l, conn):
    eq_(l._lock.user,  None)
    eq_(conn.removed, ['/var/lock/mtui.lock'])


def test_lock_doesnt_unlock():
    l = TargetLock(None, ConfigFake(dict(session_user = 'bar')))
    l.is_locked = lambda: True
    l.is_mine = lambda: False
    def x(*a, **kw):
        l.test_mark = (a, kw)
        return ""

    l.locked_by_msg = x

    rl = RemoteLock()
    rl.user = 'foo'

    l._lock = rl
    try:
        l.unlock()
    except TargetLockedError:
        pass
    else:
        ok_(False)

    eq_(l.test_mark, ((),{}))

def test_lock_force_unlock():
    conn = ConnRemovingMock()

    l = TargetLock(conn, ConfigFake(dict(session_user = 'bar')))
    l.is_locked = lambda: True
    l.is_mine = lambda: False

    rl = RemoteLock()
    rl.user = 'foo'

    l._lock = rl
    l.unlock(True)

    _unlock_post_cond(l, conn)

def _test_remote_lock_reset_lockFactory(exc_factory):
    class Lockfile:
        def readline(self):
            return "00-00:foo:666:kek"

        def close(self):
            pass

    class ConnMock(object):
        hostname = 'bar'
        cnt = 0

        def open(self, fn):
            if self.cnt == 0:
                self.cnt += 1
                return Lockfile()
            else:
                exc_factory()

    return TargetLock(ConnMock(), ConfigFake(dict(session_user = 'foo')))

def test_remote_lock_reset_on_enoent():
    def fx():
        e = IOError()
        e.errno = errno.ENOENT
        raise e
    l = _test_remote_lock_reset_lockFactory(fx)
    eq_(l.is_locked(), True)
    eq_(l.is_locked(), False)

def test_remote_lock_reset_on_exception():
    def fx():
        raise Exception("foo")
    l = _test_remote_lock_reset_lockFactory(fx)

    eq_(l.is_locked(), True)
    try:
        eq_(l.is_locked(), False)
    except Exception as e:
        eq_(e.args[0], "foo")
        eq_(l._lock.user, None)
        eq_(l._lock.timestamp, None)
        eq_(l._lock.pid, None)
        eq_(l._lock.comment, None)
