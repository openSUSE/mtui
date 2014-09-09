from .utils import LogFakeStr, LogFake
from nose.tools import eq_

def check_log_levels_are_separate(l):
    l.critical("a")
    eq_(l.criticals, ["a"])
    for i in ['infos', 'debugs', 'warnings', 'errors']:
        eq_(getattr(l, i), [])

    l.info("wat")
    eq_(l.infos, ["wat"])
    eq_(l.criticals, ["a"])

    for i in ['debugs', 'warnings', 'errors']:
        eq_(getattr(l, i), [])

def test_log_levels_are_separate():
    for i in [LogFakeStr, LogFake]:
        yield check_log_levels_are_separate,  i()
