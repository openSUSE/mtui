from nose.tools import ok_, eq_

from mtui.commands import Whoami
from mtui.config import Config
from .utils import StringIO
from .utils import SysFake

def test_whoami():
    class PromptFake:
        metadata = None

    cg = Config()
    cg.session_user = 'foo'
    c = Whoami([], [], cg, SysFake(), None, PromptFake())
    c.get_pid = lambda: 666
    c.run()
    eq_(c.sys.stdout.getvalue(), "foo 666\n")
