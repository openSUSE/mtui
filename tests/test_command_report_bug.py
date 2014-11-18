# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_

from errno import ENOENT
from subprocess import Popen

from mtui.commands import ReportBug
from mtui.prompt import CommandPrompt
from mtui.messages import SystemCommandError
from mtui.messages import SystemCommandNotFoundError
from mtui.messages import UnexpectedlyFastCleanExitFromXdgOpen
from .utils import StringIO
from .utils import ConfigFake
from .utils import LogFake
from .utils import SysFake
from .utils import unused
from .test_prompt import TestableCommandPrompt

def test_print_url():
    """
    Test url is only printed when -p is given
    """
    def raiser(*a, **kw):
        raise RuntimeError("Unexpected popen() call")

    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("-p", s), [], c, s, l,
        CommandPrompt(c, l), popen = raiser)

    cmd.run()
    eq_(cmd.sys.stdout.getvalue(), c.report_bug_url + "\n")

def test_xdg_open_happy():
    """
    Test command calls xdg open and that runs as expected
    """
    class PopenFake:
        args = []
        def __init__(self, xs):
            self.__class__.args.append(xs)

        def poll(self):
            return None

        def kill(self):
            return

    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("", s), [], c, s, l,
        CommandPrompt(c, l), popen = PopenFake)

    cmd.run()
    eq_(cmd.sys.stdout.getvalue(), "")
    eq_(PopenFake.args, [["xdg-open", c.report_bug_url]])


def test_xdg_open_failed():
    """
    Test command calls xdg open which fails
    """
    class PopenFake:
        def __init__(self, *_):
            pass

        def poll(self):
            return 6

    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("", s), [], c, s, l,
        CommandPrompt(c, l), popen = PopenFake)

    try:
        cmd.run()
    except SystemCommandError as e:
        eq_(e.command, ["xdg-open", c.report_bug_url])
        eq_(e.rc, 6)

def test_xdg_open_failed_to_exec():
    """
    Test command xdg-open is missing
    """

    def popen_fake(*_):
        raise OSError(ENOENT, unused)

    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("", s), [], c, s, l,
        CommandPrompt(c, l), popen = popen_fake)

    try:
        cmd.run()
    except SystemCommandNotFoundError as e:
        eq_(e.command, "xdg-open")

def test_xdg_open_returned_0():
    """
    Test debug message is logged when xdg-open returns 0
    """
    class PopenFake:
        def __init__(self, *_):
            pass

        def poll(self):
            return 0

    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("", s), [], c, s, l,
        CommandPrompt(c, l), popen = PopenFake)

    cmd.run()
    eq_(cmd.logger.debugs, [UnexpectedlyFastCleanExitFromXdgOpen()])

def test_completer():
    def test(in_, out):
        eq_(set(ReportBug.completer([])(*in_)), set(out))

    xs = [
        (['--', 'report-bug --', 11, 13], ["--print-url"]),
        (['-', 'report-bug -', 11, 12], ["-p", "--print-url"])
    ]
    for i, o in xs:
        yield test, i, o

def test_has_default_popen():
    c, l, s = ConfigFake(), LogFake(), SysFake()
    cmd = ReportBug(ReportBug.parse_args("", s), [], c, s, l,
        CommandPrompt(c, l))
    eq_(cmd.popen, Popen)
