# -*- coding: utf-8 -*-

from nose.tools import ok_
from nose.tools import eq_
from nose.tools import raises
from nose.tools import nottest

from mtui.prompt import CommandPrompt
from mtui.prompt import CmdQueue
from mtui.template import TestReport
from mtui.commands import Command

from .utils import LogMock
from .utils import ConfigFake

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

@nottest
class TestableCommandPrompt(CommandPrompt):
    t_read_history_called = False
    t_stop_calls = 0
    t_eof_called = False

    def __init__(self, *a, **kw):
        CommandPrompt.__init__(self, *a, **kw)
        self._add_subcommand(FakeCommandFactory())
        self.t_foo_called = []

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
        NOP overlaoad so L{sys.exit} doesn't get called
        """
        return True

    do_exit = do_quit

def test_read_history_on_init():
    c = ConfigFake()
    l = LogMock()
    cp = TestableCommandPrompt([], TestReport(c, l), c, l)
    ok_(cp.t_read_history_called)

def test_set_cmdqueue():
    c = ConfigFake()
    l = LogMock()
    cp = TestableCommandPrompt([], TestReport(c, l), c, l)
    eq_(cp.cmdqueue, [])

    cp.set_cmdqueue([])
    eq_(cp.cmdqueue, [])

    cp.set_cmdqueue(['foo', 'bar'])
    eq_(cp.cmdqueue, ['foo', 'bar'])
    ok_(isinstance(cp.cmdqueue, CmdQueue))

def test_precmd_prerun():
    c = ConfigFake()
    l = LogMock()
    cp = TestableCommandPrompt([], TestReport(c, l), c, l)
    cp.set_cmdqueue(['foo', 'bar',])
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
    c = ConfigFake()
    l = LogMock()
    cp = TestableCommandPrompt([], TestReport(c, l), c, l)
    cp.set_cmdqueue(['foo', 'ctrlc', 'stop'])
    cp.interactive = False
    cp.cmdloop()
    # FIXME: see L{test_precmd_prerun}
    ok_(cp.interactive)
    eq_(cp.cmdqueue, [])
    eq_(cp.t_stop_calls, 0)
    ok_(cp.t_eof_called)


class TestableCmdQueue(CmdQueue):
    def __init__(self, *a, **kw):
        CmdQueue.__init__(self, *a, **kw)
        self.t_echo_prompt_calls = []

    def echo_prompt(self, i):
        self.t_echo_prompt_calls.append(i)

def test_cmdqueue():
    it = [1,3,2]
    p = "prompt"
    q = TestableCmdQueue(it, p)
    eq_(q, it)
    eq_(q.prompt, p)

    el0 = q.pop(0)
    eq_(el0, 1)
    eq_(q.t_echo_prompt_calls, [1])

    el0 = q.pop(0)
    eq_(el0, 3)
    eq_(q.t_echo_prompt_calls, [1,3])
