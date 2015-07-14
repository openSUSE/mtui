# -*- coding: utf-8 -*-

from nose.tools import ok_
from nose.tools import eq_
from nose.tools import raises
from nose.tools import nottest

from mtui.prompt import CommandPrompt
from mtui.prompt import CmdQueue
from mtui.prompt import QuitLoop
from mtui.template import TestReport
from mtui.commands import Command
from mtui.types.md5 import MD5Hash
from mtui.template import SwampTestReport

from distutils.version import StrictVersion

from tests.prompt import make_cp

from .utils import LogFake
from .utils import ConfigFake
from .utils import SysFake
from .utils import unused

class FakeCommandFactory(object):
    t_run_called = 0
    t_factory_calls = 0
    command = 'bar'
    stable = '1.0'

    def parse_args(self, args, stdout):
        return []

    def __call__(self, *a, **kw):
        self.t_factory_calls += 1
        class FakeCommand(Command):
            def run(self):
                self.factory.t_run_called += 1

        c = FakeCommand(*a, **kw)
        c.factory = self
        return c

class CPDFake(object):
    def __init__(self, *a, **kw): pass

@nottest
class TestableCommandPrompt(CommandPrompt):
    t_read_history_called = False
    t_stop_calls = 0
    t_eof_called = False

    def __init__(self, config = None, log = None, sys = None):
        CommandPrompt.__init__(
            self,
            config = config or ConfigFake(),
            log = log or LogFake(),
            sys = sys or SysFake(),
            display_factory = CPDFake,
        )
        self._add_subcommand(FakeCommandFactory())
        self.t_foo_called = []
        self.t_preloop_counter = 0

    def _read_history(self):
        self.t_read_history_called = True

    def do_foo(self, line):
        self.t_foo_called.append(line)

    def do_stop(self, line):
        self.t_stop_calls += 1
        return True

    def do_ctrlc(self, line):
        raise KeyboardInterrupt

    def do_EOF(self, line):
        self.t_eof_called = True
        return True

    def do_quit(self, line):
        """
        Simulates sys.exit() inside nosetests

        :raises: QuitLoop
        """
        raise QuitLoop()

    def preloop(self):
        self.t_preloop_counter += 1

        if self.t_preloop_counter == 2:
            # used by L{test_noninteractive_drops_to_interactive_on_ctrlc}
            # to exit the cmdloop once ctrl-c was caught.
            raise QuitLoop

    do_exit = do_quit

def test_read_history_on_init():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    ok_(cp.t_read_history_called)

def test_set_cmdqueue():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    eq_(cp.cmdqueue, [])

    cp.set_cmdqueue([])
    eq_(cp.cmdqueue, [])

    cp.set_cmdqueue(['foo', 'bar'])
    eq_(cp.cmdqueue, ['foo', 'bar'])
    ok_(isinstance(cp.cmdqueue, CmdQueue))

def test_set_cmdqueue_noninteractive_prompt():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.interactive = False
    eq_(cp.cmdqueue, [])

    cp.set_cmdqueue([])
    eq_(cp.cmdqueue, ['quit'])

    cp.set_cmdqueue(['foo', 'bar'])
    eq_(cp.cmdqueue, ['foo', 'bar', 'quit'])
    ok_(isinstance(cp.cmdqueue, CmdQueue))

def test_precmd_prerun():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.set_cmdqueue(['foo', 'bar', 'quit'])
    cp.cmdloop()
    # FIXME: this may hang forever.
    # The problem is that there is no way to manually step the
    # Cmd.cmdloop as that's an infinite loop.
    # It might be good to not depend on L{cmd.Cmd} at all given the
    # modifications we need there for features (besides the unit
    # testability itself).
    # Workaround: run nosetests with >0 processes where if the cmdloop
    # starts reading stdin it will fail as the nose workers have the
    # stdin closed.
    # In case it doesn't read stdin but goes into an inifte loop somehow
    # anyway, the nose workers have default 10s timeout.

    eq_(cp.t_foo_called, [""])
    bcf = cp.commands['bar']
    eq_(bcf.t_run_called, 1)
    eq_(bcf.t_run_called,
        bcf.t_factory_calls)

def test_noninteractive_drops_to_interactive_on_ctrlc():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.set_cmdqueue(['foo', 'ctrlc', 'stop'])
    cp.interactive = False
    cp.cmdloop()
    # FIXME: see L{test_precmd_prerun}
    ok_(cp.interactive)
    eq_(cp.cmdqueue, [])
    eq_(cp.t_stop_calls, 0)
    ok_(not cp.t_eof_called)


class TestableCmdQueue(CmdQueue):
    def __init__(self, *a, **kw):
        CmdQueue.__init__(self, *a, **kw)
        self.t_echo_prompt_calls = []

    def echo_prompt(self, i):
        self.t_echo_prompt_calls.append(i)

def test_cmdqueue():
    it = [1,3,2]
    p = "prompt"
    q = TestableCmdQueue(it, p, SysFake())
    eq_(q, it)
    eq_(q.prompt, p)

    el0 = q.pop(0)
    eq_(el0, 1)
    eq_(q.t_echo_prompt_calls, [1])

    el0 = q.pop(0)
    eq_(el0, 3)
    eq_(q.t_echo_prompt_calls, [1,3])

def test_commandFactory():
    """
    Test objects get passed in to commands properly
    """
    c = ConfigFake()
    l = LogFake()
    cp = TestableCommandPrompt(c, l)

    future_config = None
    class FakeCommand(Command):
        command = 'fake'
        stable = '1.0'
        def run(self):
            self.prompt.t_cmd = self

    cp._add_subcommand(FakeCommand)
    cp.onecmd("fake")
    ok_(cp.t_cmd.prompt is cp)
    ok_(cp.t_cmd.sys is cp.sys)
    ok_(cp.t_cmd.logger is cp.log)
    ok_(cp.t_cmd.config is cp.config)
    # and some more for good measure
    ok_(cp.t_cmd.logger is l)
    ok_(cp.t_cmd.config is c)

def test_set_session_name():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.do_set_session_name("foo")
    eq_(cp.session, "foo")
    eq_(cp.prompt, "mtui:foo> ")

def test_set_session_name_auto_testreport():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    md5 = MD5Hash('8c60b7480fc521d7eeb322955b387165')
    cp.metadata = SwampTestReport(ConfigFake(), LogFake(), unused)
    cp.metadata.md5 = md5
    cp.do_set_session_name("")
    eq_(cp.prompt, "mtui:{0}> ".format(md5))
    eq_(cp.session, md5)

def test_set_session_name_auto_no_testreport():
    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.do_set_session_name("")
    eq_(cp.prompt, "mtui> ")
    eq_(cp.session, None)

def test_load_update_doesnt_leave_previous_session():
    class FakeUpdate:
        def make_testreport(self):
            md5 = MD5Hash('11111111111111111111111111111111')
            return SwampTestReport(ConfigFake(), LogFake(), unused)

    cp = TestableCommandPrompt(ConfigFake(), LogFake())
    cp.metadata = SwampTestReport(ConfigFake(), LogFake(), unused)
    cp.metadata.md5 = MD5Hash('00000000000000000000000000000000')
    cp.load_update(FakeUpdate(), autoconnect=False)
    eq_(cp.prompt, "mtui> ")
    eq_(cp.session, None)

def test_set_location():
    p = make_cp()
    loc = 'foolocation'
    ok_(p.config.location != loc)
    p.do_set_location(loc)
    eq_(p.config.location, loc)
