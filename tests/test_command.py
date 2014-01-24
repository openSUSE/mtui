from nose.tools import ok_, eq_

from mtui.prompt import CommandPrompt
from mtui.target import Metadata
from mtui.commands import Whoami
from mtui.config import Config, config
from mtui import __version__

import os
from distutils.version import StrictVersion

OLD_STYLE_CMD='update'
NEW_STYLE_CMD='unlock'

class MyStdout:
    def write(self, x):
        self.written = x

def test_do_help():
    c = Config()
    c.command_interface = '2.0'
    cp = CommandPrompt([], Metadata(), config=c)
    cp.stdout = MyStdout()
    cp.do_help(NEW_STYLE_CMD)
    ok_('usage: '+NEW_STYLE_CMD in cp.stdout.written)

    cp.do_help(OLD_STYLE_CMD)
    # just that doesn't raise

def test_getattr():
    c = Config()
    c.command_interface = '2.0'
    cp = CommandPrompt([], Metadata(), config=c)
    attr = "do_"+NEW_STYLE_CMD
    r = getattr(cp, attr)
    ok_('usage: '+NEW_STYLE_CMD in r.__doc__)

    attr = 'do_'+OLD_STYLE_CMD
    r = getattr(cp, attr)
    ok_(r == cp.do_update)

def test_getnames():
    c = Config()
    c.command_interface = '2.0'
    cp = CommandPrompt([], Metadata(), config=c)
    names = cp.get_names()
    ok_('do_'+NEW_STYLE_CMD in names)
    ok_('do_'+OLD_STYLE_CMD in names)

def test_command_prompt_init():
    c = Config()
    cp = CommandPrompt([], Metadata())
    ok_(cp.config is config)

    os.environ['MTUI_CONF'] = '/dev/null'
    c = Config()
    ok_(not c is config)
    cp = CommandPrompt([], Metadata(), config=c)
    ok_(c is cp.config)

    eq_(cp._command_interface, StrictVersion(__version__))

def test_add_subcommand():
    class TestableCP(CommandPrompt):
        def __init__(self):
            self._command_interface = StrictVersion('1.1.0')
            self.commands = {}

    cp = TestableCP()

    class TestableComm(Whoami):
        stable = '2.0'

    eq_(cp.commands.values(), [])
    cp._add_subcommand(TestableComm)
    eq_(cp.commands.values(), [])

    class TestableCP2(CommandPrompt):
        def __init__(self):
            self._command_interface = StrictVersion('2.0')
            self.commands = {}

    cp = TestableCP2()
    eq_(cp.commands.values(), [])
    cp._add_subcommand(TestableComm)
    eq_(cp.commands.values(), [TestableComm])
