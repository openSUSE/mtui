# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_
from nose.tools import nottest

from mtui.main import get_parser
from mtui.main import run_mtui
from .utils import ConfigFake
from .utils import StringIO
from .utils import OneShotFactory
from .utils import LogMock

# TODO: check the args get passed correctly into the application once
# the main() was refactored enough

def test_argparser_sut():
    # FIXME: parse SUTs as part of the parser
    p = get_parser()
    a = p.parse_args(["-s", "foo", "--sut", "bar"])
    eq_(a.sut, ["foo", "bar"])

def test_argparser_autoadd():
    # TODO: validate attributes
    p = get_parser()
    a = p.parse_args(["-a", "foo", "--autoadd", "bar"])
    eq_(a.autoadd, ["foo", "bar"])

class PromptFake(object):
    def __init__(self, targets, test_report, config, log):
        self.targets = targets
        self.metadata = test_report
        self.config = config
        self.log = log
        self.interactive = True

        self.t_autoadds = []
        self.t_cmdloops = 0
        self.t_cmdqueues = []

    def do_autoadd(self, line):
        self.autoadds.append(line)

    def cmdloop(self):
        self.t_cmdloops += 1

    def set_cmdqueueu(self, queue):
        self.t_cmdqueues.append(queue)

@nottest
class TestReportFake(object):
    def __init__(self, config, log):
        self.config = config
        self.log = log
        self.t_load_systems_from_testplatforms = 0
        self.t_connect_targets = 0

    def load_systems_from_testplatforms(self):
        self.t_load_systems_from_testplatforms += 1

    def connect_targets(self):
        self.t_connect_targets += 1

@nottest
class TestReportFactoryFake(OneShotFactory):
    def __init__(self):
        super(TestReportFactoryFake, self).__init__(TestReportFake)

    def _make_product(self, config, log, md5=None):
        return TestReportFake(config, log)

class SysFake(object):
    def __init__(self, argv):
        self.argv = argv
        self.stdout = StringIO()

def test_main():
    c = ConfigFake()
    trff = TestReportFactoryFake()
    pf = OneShotFactory(PromptFake)
    lf = LogMock()
    sysf = SysFake(["mtui"])
    ok_(run_mtui(sysf, c, lf, trff, pf) is 0)

    prompt = pf.product
    testreport = trff.product

    eq_(sysf.stdout.getvalue(), "")
    eq_(prompt.t_cmdloops, 1)
    eq_(prompt.t_cmdqueues, [])
    eq_(prompt.interactive, True)
    eq_(prompt.t_autoadds, [])
    eq_(testreport.t_load_systems_from_testplatforms, 1)
    eq_(testreport.t_connect_targets, 1)

def test_main_config_overrides():
    location = 'prague'
    template_dir = '/home/foo/bar/'
    timeout = '666'

    c = ConfigFake()

    overrides = [
      (location, lambda: c.location)
    , (template_dir, lambda: c.template_dir)
    , (int(timeout), lambda: c.connection_timeout)
    ]

    for x, y in overrides:
        ok_(x != y(), "tautological setup")

    trff = TestReportFactoryFake()
    pf = OneShotFactory(PromptFake)
    lf = LogMock()

    sysf = SysFake([
      "mtui"
    , "-l", location
    , "-t", template_dir
    , "-w", timeout
    ])

    ok_(run_mtui(sysf, c, lf, trff, pf) is 0)

    prompt = pf.product
    testreport = trff.product

    eq_(sysf.stdout.getvalue(), "")
    eq_(prompt.t_cmdloops, 1)
    eq_(prompt.t_cmdqueues, [])
    eq_(prompt.interactive, True)
    eq_(prompt.t_autoadds, [])
    eq_(testreport.t_load_systems_from_testplatforms, 1)
    eq_(testreport.t_connect_targets, 1)

    for x, y in overrides:
        eq_(x, y(), "override didn't take effect: {0} != {1}".format(x, y))
