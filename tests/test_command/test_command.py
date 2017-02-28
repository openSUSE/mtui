"""
This module is concerned with the code providing framework to implement
commands as classes.

That is

1. L{Command} class itself.

2. L{CommandPrompt}s adding & executing of L{Command}
   classes.
"""

from nose.tools import ok_, eq_

from mtui.commands import Command

from tests.prompt import make_cp

from ..utils import SysFake
from ..utils import unused
from ..utils import PromptFake
import collections


OLD_STYLE_CMD='update'
NEW_STYLE_CMD='unlock'


class ComMock2_0(Command):
    command = 'commock'

def test_do_help():
    """
    Test CommandPrompt.do_help print helps properly for class-defined
    commands.
    """
    cp = make_cp()

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

    cmd = ComMock2_0.command
    cp = make_cp()

    ok_(not hasattr(cp, 'do_%s' % cmd), cmd)
    cp._add_subcommand(ComMock2_0)

    for p in 'complete_', 'do_', 'help_':
        attr = p + cmd
        ok_(isinstance(getattr(cp, attr), collections.Callable), attr)

def test_getnames():
    """
    Test L{CommandPrompt.getnames} returns commands including the
    class-defined ones.
    """
    cp = make_cp()
    attr = "do_"+ComMock2_0.command
    ok_(attr not in cp.get_names())
    cp._add_subcommand(ComMock2_0)
    names = cp.get_names()
    ok_(attr in names)
    ok_('do_'+OLD_STYLE_CMD in names)

def test_add_subcommand():
    """
    Test L{CommandPrompt._add_subcommand} adds class-defined commands
    """
    cp = make_cp()
    ok_(ComMock2_0 not in list(cp.commands.values()))
    cp._add_subcommand(ComMock2_0)
    ok_(ComMock2_0 in list(cp.commands.values()))

def test_command_argparse_fail():
    """
    Test handling of command args parsing failure
    namely that ArgsParseFailure is caught during onecmd() and therefore
    the command itself is NOT executed.
    """
    class ComMock(Command):
        command = 'commock'

        def run(self):
            ok_(False)

    cp = make_cp()
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

    cp = make_cp()
    cp._add_subcommand(ComMock)
    cp.onecmd('commock -h')
    eq_(cp.sys.stdout.getvalue(), 'usage: commock [-h]\n\noptional '+
        'arguments:\n  -h, --help  show this help message and exit\n')

def test_command_println():
    class ComMock(Command):
        def run(self):
            pass

    c = ComMock(None, None, None, SysFake(unused), None, PromptFake())
    c.println("a")
    eq_(c.sys.stdout.getvalue(), "a\n")
