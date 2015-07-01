from mtui.target import HostsGroup, Target
from nose.tools import ok_, eq_
from nose.tools import raises

from mtui.messages import HostIsNotConnectedError

def make_target(hostname):
    return Target(hostname, None, connect = False)

def test_select_hosts():
    a = make_target('a')
    b = make_target('b')
    c = make_target('c')

    hg = HostsGroup([a, b, c])

    hg2 = hg.select(['a', 'c'])
    ok_(isinstance(hg2, HostsGroup))
    eq_(len(hg2.hosts), 2)
    ok_(a in hg2.hosts.values())
    ok_(c in hg2.hosts.values())

def test_select_nohosts():
    a = make_target('a')

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
