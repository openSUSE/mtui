from nose.tools import eq_

from mtui.commands import HostsUnlock

from ..utils import unused


def test_unlock_parser():
    hosts = ["-t foo.suse.de", "-t bar.suse.de"]
    cleanhosts = ["foo.suse.de", "bar.suse.de"]
    args = HostsUnlock.parse_args(" ".join(hosts), unused)
    eq_(args.hosts, cleanhosts)
