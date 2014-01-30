from nose.tools import ok_, eq_

from mtui.prompt import CommandPrompt
from mtui.target import Metadata
from mtui.commands import Command
from mtui.config import Config, config
from mtui import __version__

import os
import argparse
from distutils.version import StrictVersion

try:
    from cStringIO import StringIO
except ImportError:
    from StringIO import StringIO

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

    class ComMock(Command):
        stable = '2.0'
        command = 'commock'

    eq_(cp.commands.values(), [])
    cp._add_subcommand(ComMock)
    eq_(cp.commands.values(), [])

    class TestableCP2(CommandPrompt):
        def __init__(self):
            self._command_interface = StrictVersion('2.0')
            self.commands = {}

    cp = TestableCP2()
    eq_(cp.commands.values(), [])
    cp._add_subcommand(ComMock)
    eq_(cp.commands.values(), [ComMock])

def test_command_argparse_fail():
    """
    test handling of command args parsing failure
    namely that ArgsParseFailure is catched during onecmd()
    """
    class ComMock(Command):
        stable = '1.0'
        command = 'commock'

        def run(self):
            ok_(False)

    c = Config()
    cp = CommandPrompt([], Metadata(), config=c)
    cp.stdout = StringIO()
    cp._add_subcommand(ComMock)

    cp.onecmd('commock -foo')
    eq_(cp.stdout.getvalue(), "usage: commock [-h]\n")

def test_command_doesnt_run_on_help():
    class ComMock(Command):
        command = 'commock'
        stable = '2.0'

        def run(self):
            ok_(False)

    c = Config()
    c.command_interface = ComMock.stable
    cp = CommandPrompt([], Metadata(), config=c)
    cp.stdout = StringIO()
    cp._add_subcommand(ComMock)
    cp.onecmd('commock -h')
    eq_(cp.stdout.getvalue(), 'usage: commock [-h]\n\noptional '+
        'arguments:\n  -h, --help  show this help message and exit\n')

def test_command_println():
    class ComMock(Command):
        def run(self):
            pass

    c = ComMock(None, None, None, StringIO(), None)
    c.println("a")
    eq_(c.stdout.getvalue(), "a\n")
