from nose.tools import ok_, eq_, raises

from paramiko import SSHException

from mtui import prompt
from mtui.target import TargetLock, RemoteLock, Target
from mtui.target import LockedTargets
from mtui import messages
from mtui.connection import Connection
from .utils import ConfigFake
from .utils import LogFake
from .utils import LogFakeStr
from .utils import unused
from .utils import hostnames
from .utils import StringIO

def TF(hostname, lock = None, connection = None, logger = None
, state = 'enabled', connect = True):
    """
    TargetFactory
    """

    kw = dict(logger = logger if logger else LogFake())
    if lock:
        kw['lock'] = lock
    if connection:
        kw['connection'] = connection

    kw['state'] = state
    kw['connect'] = connect
    return Target(hostname, unused, **kw)

def test_legacy_locked_target_is_locked():
    t = TF('foo', connect = False, logger = LogFake())

    t._lock = TargetLock(None, ConfigFake(dict(session_user = 'foo')), LogFake())
    t._lock.load = lambda: None

    rl = RemoteLock()
    rl.user = 'quux'
    rl.pid = 666
    rl.timestamp = 'bah'
    rl.comment  = 'fuu'
    t._lock._lock = rl

    lock = t.locked()
    eq_(lock.locked, True)
    eq_(lock.user, 'quux')
    eq_(lock.pid, '666')
    eq_(lock.comment, 'fuu')
    eq_(lock.timestamp, rl.timestamp)
    eq_(lock.own(), False)

def test_legacy_lock_is_own():
    t = TF('foo', connect = False, logger = LogFake())

    t._lock = TargetLock(None, ConfigFake(dict(session_user = 'quux')), LogFake())
    t._lock.load = lambda: None

    rl = RemoteLock()
    rl.user = 'quux'
    rl.pid = 666
    rl.timestamp = 'bah'
    rl.comment  = 'fuu'
    t._lock._lock = rl

    lock = t.locked()
    lock._getpid = lambda: 666
    lock._getuser = lambda: 'quux'
    eq_(lock.locked, True)
    eq_(lock.user, 'quux')
    eq_(lock.pid, '666')
    eq_(lock.comment, 'fuu')
    eq_(lock.timestamp, rl.timestamp)
    eq_(lock.own(), True)


def test_legacy_target_set_locks():
    t = TF('foo', connect = False, logger = LogFake())

    t._lock = TargetLock(None, ConfigFake(dict(session_user = 'foo')), LogFake())
    t._lock.load = lambda: None
    def lock(*a, **kw):
        t.locked_with = (a, kw)
    t._lock.lock = lock

    t.set_locked('foo')
    eq_(t.locked_with, (('foo',), {}))

def test_legacy_target_remove_lock_on_enabled():
    t = TF('foo', connect = False, logger = LogFake())
    eq_(t.state, "enabled")

    t.unlock_called = False
    def x():
        t.unlock_called = True
    t.unlock = x

    t.remove_lock()
    eq_(t.unlock_called, True)


def test_legacy_target_remove_lock_on_disabled():
    t = TF('foo', connect = False, logger = LogFake())
    t.state = 'disabled'

    # FIXME: this will easily yield false negative
    t.unlock_called = False
    def x():
        t.unlock_called = True
    t.unlock = x

    t.remove_lock()
    eq_(t.unlock_called, False)

def test_target_unlock():
    t = TF('foo', connect = False, logger = LogFake())
    t.state = None
    # state is irrelevant

    t._lock = TargetLock(None, ConfigFake(dict(session_user = 'foo')), LogFake())
    t._lock.load = lambda: None

    def x(*a, **kw):
        t.test_mark = (a, kw)

    t._lock.unlock = x
    t.unlock()

    eq_(t.test_mark, ((False,),{})) # ((force), {})

def test_locked_target_is_locked():
    t = TF('foo', connect = False, logger = LogFake())

    t._lock = TargetLock(None, ConfigFake(dict(session_user = 'foo')), LogFake())
    t._lock.is_locked = lambda: False
    t._lock.i_am_pid = 666
    t._lock.timestamp_factory = lambda: '00-00'

    def x(*a, **kw):
        t.test_mark = (a, kw)
    t._lock.lock = x

    t.lock('fuu')
    eq_(t.test_mark, (('fuu',), {}))

def test_put_repclean_fail():
    t = TF('foo', connect = False, logger = LogFake())
    t.logger = LogFake()
    def put():
        raise Exception()
    t.put = put
    t._upload_repclean()
    exp_errors = ['rep-clean uploading failed please see BNC#860284']
    eq_(t.logger.errors, exp_errors)

class TestTargetConnect(object):
    def test_happy_path(self):
        t = TF("foo", connection = ConnectionFake, lock = TargetLockFake)
        ok_(t.connection)

    def test_connection_error(self):
        class ConnFake(ConnectionFake):
            def connect(self):
                raise SSHException("bar")

        l = LogFakeStr()
        try:
            t = TF(hostnames.foo, connection = ConnFake, logger = l, lock = TargetLockFake)
        except SSHException:
            eq_(l.criticals, [
                str(messages.ConnectingTargetFailedMessage(hostnames.foo, "bar"))
            ])
            eq_(l.infos, [
                str(messages.ConnectingToMessage(hostnames.foo))
            ])
        else:
            ok_(False, "exception expected")

    def test_host_locked(self):
        class ConnFake(ConnectionFake):
            def open(self, filename, mode = 'r', bufsize = -1):
                rl = RemoteLock()
                rl.user = "alice"
                rl.timestamp = "0000"
                rl.pid = 666
                return StringIO(rl.to_lockfile())

        class LockFake(TargetLockFake):
            def __init__(self, *a, **kw):
                super(LockFake, self).__init__(*a, **kw)
                self.lock()

        t = TF(hostnames.foo, connection = ConnFake, lock = TargetLock)
        eq_(t.logger.warnings, ["{0} is locked by alice.".format(hostnames.foo)])

class TargetLockFake(object):
    def __init__(self, conn, config, log):
        self.unlock()

    def lock(self, comment = None):
        self.locked = True

    def unlock(self, force = None):
        self.locked = False

    def is_locked(self):
        return self.locked

class ConnectionFake(Connection):
    def connect(self):
        pass
    def load_keys(self):
        pass

class MyErr(RuntimeError):
    pass

class TestLockedTargets(object):
    def _check_locks(self, targets, result):
        if not targets:
            raise ValueError("no targets")

        for t in targets:
            eq_(t.is_locked(), result)

    def test_happy_path(self):
        ts = [
            TF(hostnames.foo, lock = TargetLockFake, connection = ConnectionFake),
            TF(hostnames.bar, lock = TargetLockFake, connection = ConnectionFake),
            TF(hostnames.qux, lock = TargetLockFake, connection = ConnectionFake),
        ]

        self._check_locks(ts, False)
        with LockedTargets(ts):
            self._check_locks(ts, True)

        self._check_locks(ts, False)

    def test_reraise(self):
        ts = [
            TF(hostnames.foo, lock = TargetLockFake, connection = ConnectionFake),
            TF(hostnames.bar, lock = TargetLockFake, connection = ConnectionFake),
            TF(hostnames.qux, lock = TargetLockFake, connection = ConnectionFake),
        ]

        self._check_locks(ts, False)
        m = "foo"
        try:
            with LockedTargets(ts):
                self._check_locks(ts, True)
                raise MyErr(m)
        except MyErr as e:
            eq_(str(e), m)
            self._check_locks(ts, False)
