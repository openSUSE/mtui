from mtui.target import HostsGroup, Target
from nose.tools import ok_, eq_

def test_select_hosts():
    a = Target('a', None, connect=False)
    b = Target('b', None, connect=False)
    c = Target('c', None, connect=False)

    hg = HostsGroup([a, b, c])

    hg2 = hg.select(['a', 'c'])
    ok_(isinstance(hg2, HostsGroup))
    eq_(len(hg2.hosts), 2)
    ok_(a in hg2.hosts)
    ok_(c in hg2.hosts)

def test_select_nohosts():
    a = Target('a', None, connect=False)

    hg = HostsGroup([a])

    hg2 = hg.select([])
    ok_(isinstance(hg2, HostsGroup))
    ok_(hg2 is hg)
