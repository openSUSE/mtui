from nose.tools import ok_, eq_, raises

from mtui.target import TargetLock, RemoteLock, Target
from mtui.config import Config
from .utils import LogMock


def test_legacy_locked_target_is_locked():
    t = Target('foo', 'bar', connect=False)

    c = Config
    c.session_user = 'foo'

    t._lock = TargetLock(None, c)
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
    t = Target('foo', 'bar', connect=False)

    c = Config
    c.session_user = 'quux'

    t._lock = TargetLock(None, c)
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
    t = Target('foo', 'bar', connect=False)

    c = Config
    c.session_user = 'foo'

    t._lock = TargetLock(None, c)
    t._lock.load = lambda: None
    def lock(*a, **kw):
        t.locked_with = (a, kw)
    t._lock.lock = lock

    t.set_locked('foo')
    eq_(t.locked_with, (('foo',), {}))

def test_legacy_target_remove_lock_on_enabled():
    t = Target('foo', 'bar', connect=False)
    eq_(t.state, "enabled")

    t.unlock_called = False
    def x():
        t.unlock_called = True
    t.unlock = x

    t.remove_lock()
    eq_(t.unlock_called, True)


def test_legacy_target_remove_lock_on_disabled():
    t = Target('foo', 'bar', connect=False)
    t.state = 'disabled'

    # FIXME: this will easily yield false negative
    t.unlock_called = False
    def x():
        t.unlock_called = True
    t.unlock = x

    t.remove_lock()
    eq_(t.unlock_called, False)

def test_target_unlock():
    t = Target('foo', 'bar', connect=False)
    t.state = None
    # state is irrelevant

    c = Config
    c.session_user = 'foo'

    t._lock = TargetLock(None, c)
    t._lock.load = lambda: None

    def x(*a, **kw):
        t.test_mark = (a, kw)

    t._lock.unlock = x
    t.unlock()

    eq_(t.test_mark, ((False,),{})) # ((force), {})

def test_locked_target_is_locked():
    t = Target('foo', 'bar', connect=False)

    c = Config
    c.session_user = 'foo'

    t._lock = TargetLock(None, c)
    t._lock.is_locked = lambda: False
    t._lock.i_am_pid = 666
    t._lock.timestamp_factory = lambda: '00-00'

    def x(*a, **kw):
        t.test_mark = (a, kw)
    t._lock.lock = x

    t.lock('fuu')
    eq_(t.test_mark, (('fuu',), {}))

def test_put_repclean_fail():
    t = Target('foo', 'bar', connect=False)
    t.logger = LogMock()
    def put():
        raise Exception()
    t.put = put
    t._upload_repclean()
    exp_errors = ['rep-clean uploading failed please see BNC#860284']
    eq_(t.logger.errors, exp_errors)
