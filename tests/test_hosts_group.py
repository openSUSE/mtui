from mtui.target import HostsGroup, Target
from nose.tools import ok_, eq_
from nose.tools import raises

from mtui.messages import HostIsNotConnectedError

def test_select_hosts():
    a = Target('a', None, connect=False)
    b = Target('b', None, connect=False)
    c = Target('c', None, connect=False)

    hg = HostsGroup([a, b, c])

    hg2 = hg.select(['a', 'c'])
    ok_(isinstance(hg2, HostsGroup))
    eq_(len(hg2.hosts), 2)
    ok_(a in hg2.hosts.values())
    ok_(c in hg2.hosts.values())

def test_select_nohosts():
    a = Target('a', None, connect=False)

    hg = HostsGroup([a])

    hg2 = hg.select([])
    ok_(isinstance(hg2, HostsGroup))
    ok_(hg2 is hg)

@raises(HostIsNotConnectedError)
def test_select_unavailable_target():
    hg = HostsGroup([])
    hg.select(["unavailable"])

@raises(ValueError)
def test_select_unavailable_target_ve():
    # for backwards compatibility check the new exception is catchable
    # as ValueError as well
    hg = HostsGroup([])
    hg.select(["unavailable"])
