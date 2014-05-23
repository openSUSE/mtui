from nose.tools import ok_, eq_

from mtui.commands import Whoami
from mtui.config import Config
from .utils import StringIO


def test_whoami():
    cg = Config()
    cg.session_user = 'foo'
    c = Whoami([], [], cg, StringIO(), None)
    c.get_pid = lambda: 666
    c.run()
    eq_(c.stdout.getvalue(), "foo 666\n")
