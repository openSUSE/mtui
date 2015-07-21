from nose.tools import ok_, eq_

from mtui.commands import HostsUnlock

from .utils import unused

def test_unlock_parser():
    hosts = ["foo.suse.de", "bar.suse.de"]
    args = HostsUnlock.parse_args(" ".join(hosts), unused)
    eq_(args.hosts, hosts)
