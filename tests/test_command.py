from nose.tools import ok_, eq_

from mtui.prompt import CommandPrompt
from mtui.target import Metadata

OLD_STYLE_CMD='update'
NEW_STYLE_CMD='unlock'

class MyStdout:
    def write(self, x):
        self.written = x

def test_do_help():
    cp = CommandPrompt([], Metadata())
    cp.stdout = MyStdout()
    cp.do_help(NEW_STYLE_CMD)
    ok_('usage: '+NEW_STYLE_CMD in cp.stdout.written)

    cp.do_help(OLD_STYLE_CMD)
    # just that doesn't raise

def test_getattr():
    cp = CommandPrompt([], Metadata())
    attr = "do_"+NEW_STYLE_CMD
    r = getattr(cp, attr)
    ok_('usage: '+NEW_STYLE_CMD in r.__doc__)

    attr = 'do_'+OLD_STYLE_CMD
    r = getattr(cp, attr)
    ok_(r == cp.do_update)

def test_getnames():
    cp = CommandPrompt([], Metadata())
    names = cp.get_names()
    ok_('do_'+NEW_STYLE_CMD in names)
    ok_('do_'+OLD_STYLE_CMD in names)
