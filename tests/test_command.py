from nose.tools import ok_, eq_

from mtui.prompt import CommandPrompt
from mtui.commands import Command
from mtui.config import Config, config
from mtui.template import TestReport
from mtui import __version__

import os
import argparse
from distutils.version import StrictVersion

from .utils import LogMock
from .utils import StringIO

OLD_STYLE_CMD='update'
NEW_STYLE_CMD='unlock'


class ComMock2_0(Command):
    stable = '2.0'
    command = 'commock'

class MyStdout:
    def write(self, x):
        self.written = x

def test_do_help():
    c = Config()
    c.interface_version = ComMock2_0.stable
    l = LogMock()
    cp = CommandPrompt([], TestReport(c, l), c, l)
    cp.stdout = MyStdout()

    cp.do_help("commock")
    eq_(cp.stdout.written, "*** No help on commock\n")

    cp._add_subcommand(ComMock2_0)
    cp.do_help("commock")
    ok_('usage: '+ComMock2_0.command in cp.stdout.written)

    cp.do_help(OLD_STYLE_CMD)
    # just that doesn't raise

def test_getattr():
    """
    Test L{CommandPrompt.getattr} simulates existence of
    CommandPrompt.do_<command> methods for commands defined by classes.

    Because that's what L{cmd.Cmd} resolves command names to.
    """
    c = Config()
    c.interface_version = ComMock2_0.stable
    l = LogMock()

    cp = CommandPrompt([], TestReport(c, l), c, l)
    attr="do_"+ComMock2_0.command
    ok_(not hasattr(cp, attr))

    cp._interface_version = StrictVersion(ComMock2_0.stable)
    cp._add_subcommand(ComMock2_0)
    r = getattr(cp, attr)
    ok_('usage: '+ComMock2_0.command in r.__doc__)

    attr = 'do_'+OLD_STYLE_CMD
    r = getattr(cp, attr)
    ok_(r == cp.do_update)

def test_getnames():
    c = Config()
    c.interface_version = ComMock2_0.stable
    l = LogMock()
    cp = CommandPrompt([], TestReport(c, l), c, l)
    attr = "do_"+ComMock2_0.command
    ok_(attr not in cp.get_names())
    cp._add_subcommand(ComMock2_0)
    names = cp.get_names()
    ok_(attr in names)
    ok_('do_'+OLD_STYLE_CMD in names)

def test_command_prompt_init():
    os.environ['MTUI_CONF'] = '/dev/null'
    l = LogMock()
    c = Config()
    cp = CommandPrompt([],  TestReport(c, l), c, l)
    ok_(c is cp.config)

    eq_(cp._interface_version, StrictVersion(__version__))

def test_add_subcommand():
    class TestableCP(CommandPrompt):
        def __init__(self):
            # FIXME: inits are overriden to prevent definition of
            # production commands and faking of other dependencies
            self.commands = {}

    cp = TestableCP()
    # set lower version than ComMock2_0
    self._interface_version = StrictVersion(
        ".".join([x for x in map(add, ComMock2_0.stable, (-1, 1))])
    )

    eq_(cp.commands.values(), [])
    cp._add_subcommand(ComMock2_0)
    eq_(cp.commands.values(), [])

    cp = TestableCP()
    cp._interface_version = StrictVersion(ComMock2_0.stable)
    eq_(cp.commands.values(), [])
    cp._add_subcommand(ComMock2_0)
    eq_(cp.commands.values(), [ComMock2_0])

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
    l = LogMock()
    cp = CommandPrompt([], TestReport(c, l), c, l)
    cp.stdout = StringIO()
    cp._add_subcommand(ComMock)

    cp.onecmd('commock -foo')
    eq_(cp.stdout.getvalue(), "usage: commock [-h]\n")

def test_command_doesnt_run_on_help():
    class ComMock(ComMock2_0):
        def run(self):
            ok_(False)

    c = Config()
    c.interface_version = ComMock.stable
    l = LogMock()
    cp = CommandPrompt([], TestReport(c, l), c, l)
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
