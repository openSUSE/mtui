from nose.tools import eq_

from mtui.commands import Whoami
from ..utils import ConfigFake
from ..utils import SysFake
from ..utils import PromptFake

def test_whoami():

    cg = ConfigFake(dict(session_user = 'foo'))
    c = Whoami([], [], cg, SysFake(), None, PromptFake())
    c.get_pid = lambda: 666
    c.run()
    eq_(c.sys.stdout.getvalue(), "foo 666\n")
