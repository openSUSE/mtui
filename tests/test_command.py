"""
This module is concerned with the code providing framework to implement
commands as classes.

That is

1. L{Command} class itself.

2. L{CommandPrompt}s handling (adding & executing) of L{Command}
   classes. Especially with regard to the
   L{mtui.config.interface_version} feature.
"""

from nose.tools import ok_, eq_

from mtui.prompt import CommandPrompt
from mtui.commands import Command
from mtui.config import Config, config
from mtui.template import TestReport
from mtui import __version__
from operator import add

import os
import argparse
from distutils.version import StrictVersion

from .utils import LogFake
from .utils import ConfigFake
from .utils import StringIO
from .utils import SysFake
from .utils import unused

OLD_STYLE_CMD='update'
NEW_STYLE_CMD='unlock'


class ComMock2_0(Command):
    stable = '2.0'
    command = 'commock'

def test_do_help():
    """
    Test CommandPrompt.do_help print helps properly for class-defined
    commands.
    """
    c = Config()
    c.interface_version = ComMock2_0.stable
    cp = CommandPrompt(c, LogFake(), SysFake())

    cp.do_help("commock")
    eq_(cp.sys.stdout.getvalue(), "*** No help on commock\n")

    cp._add_subcommand(ComMock2_0)
    cp.do_help("commock")
    ok_('usage: '+ComMock2_0.command in cp.sys.stdout.getvalue())

    cp.do_help(OLD_STYLE_CMD)
    # just that doesn't raise

def test_getattr():
    """
    Test L{CommandPrompt.getattr} simulates existence of
    CommandPrompt.do_<command> methods for class-defined commands.

    Because that's what L{cmd.Cmd} resolves command names to.
    """
    c = Config()
    c.interface_version = ComMock2_0.stable

    cp = CommandPrompt(c, LogFake())
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
    """
    Test L{CommandPrompt.getnames} returns commands including the
    class-defined ones.
    """
    c = Config()
    c.interface_version = ComMock2_0.stable
    cp = CommandPrompt(c, LogFake())
    attr = "do_"+ComMock2_0.command
    ok_(attr not in cp.get_names())
    cp._add_subcommand(ComMock2_0)
    names = cp.get_names()
    ok_(attr in names)
    ok_('do_'+OLD_STYLE_CMD in names)

def test_command_prompt_init():
    """
    Test L{CommandPrompt} is initialized with interface_version =
    current mtui version unless defined by config.
    """
    os.environ['MTUI_CONF'] = '/dev/null'
    c = Config()
    cp = CommandPrompt(c, LogFake())
    ok_(c is cp.config)

    eq_(cp._interface_version, StrictVersion(__version__))

def test_add_subcommand():
    """
    Test L{CommandPrompt._add_subcommand} handles (adds or skips)
    class-defined commands properly with regard to requested
    interface_version
    """

    class TestableCP(CommandPrompt):
        def __init__(self):
            # FIXME: inits are overriden to prevent definition of
            # production commands and faking of other dependencies
            self.commands = {}

    cp = TestableCP()
    # set lower version than ComMock2_0
    cp._interface_version = StrictVersion(
        ".".join([str(x) for x in map(add,
            StrictVersion(ComMock2_0.stable).version, (-1, 1, 0))])
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
    Test handling of command args parsing failure
    namely that ArgsParseFailure is caught during onecmd() and therefore
    the command itself is NOT executed.
    """
    class ComMock(Command):
        stable = '1.0'
        # only to keep the interface.
        # class-defined commands were introduced in >1.0,
        # therefore this command should always be active.
        command = 'commock'

        def run(self):
            ok_(False)

    cp = CommandPrompt(ConfigFake(), LogFake(), SysFake())
    cp._add_subcommand(ComMock)

    cp.onecmd('commock -foo')
    eq_(cp.sys.stdout.getvalue(), "usage: commock [-h]\n")

def test_command_doesnt_run_on_help():
    """
    Test command itself is not executed if help is requested.
    """
    class ComMock(ComMock2_0):
        def run(self):
            ok_(False)

    c = ConfigFake()
    c.interface_version = ComMock.stable
    cp = CommandPrompt(c, LogFake(), SysFake())
    cp._add_subcommand(ComMock)
    cp.onecmd('commock -h')
    eq_(cp.sys.stdout.getvalue(), 'usage: commock [-h]\n\noptional '+
        'arguments:\n  -h, --help  show this help message and exit\n')

def test_command_println():
    class ComMock(Command):
        def run(self):
            pass

    c = ComMock(None, None, None, SysFake(unused), None, None)
    c.println("a")
    eq_(c.sys.stdout.getvalue(), "a\n")
