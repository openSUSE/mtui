# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_
from nose.tools import raises
from nose.tools import nottest

from mtui.main import get_parser
from mtui.main import run_mtui
from .utils import ConfigFake
from .utils import SysFake
from .utils import StringIO
from .utils import OneShotFactory
from .utils import LogFake
from .utils import rand_maintenance_id
from .utils import rand_review_id
from mtui.argparse import ArgsParseFailure
from mtui.types.obs import RequestReviewID

# TODO: check the args get passed correctly into the application once
# the main() was refactored enough

def test_argparser_sut():
    # FIXME: parse SUTs as part of the parser
    p = get_parser(SysFake())
    a = p.parse_args(["-s", "foo", "--sut", "bar"])
    eq_(a.sut, ["foo", "bar"])

def test_argparser_autoadd():
    # TODO: validate attributes
    p = get_parser(SysFake())
    a = p.parse_args(["-a", "foo", "--autoadd", "bar"])
    eq_(a.autoadd, ["foo", "bar"])

def helper_parse_reviewid(rrid):
    return get_parser(SysFake()).parse_args(
        [ "-r"
        , rrid
        ])

def test_argparser_reviewid_ok():
    """
    Test correct RRID is parsed successfully
    """

    rrid = RequestReviewID(
        rand_maintenance_id(),
        rand_review_id()
    )

    parsed = helper_parse_reviewid(str(rrid)).review_id
    eq_(parsed.review_id, rrid.review_id)
    eq_(parsed.maintenance_id, rrid.maintenance_id)

@raises(ArgsParseFailure)
def test_parse_rrid_w0():
    """
    Test parse failure: missing rid
    """
    helper_parse_reviewid("SUSE:Maintenance:1:")

@raises(ArgsParseFailure)
def test_parse_rrid_w1():
    """
    Test parse failure: missing mid
    """
    helper_parse_reviewid("SUSE:Maintenance:")

@raises(ArgsParseFailure)
def test_parse_rrid_w2():
    """
    Test parse failure: md5 sum instead
    """
    helper_parse_reviewid("a93bcc098674a50ea93791fc528bdd9f")

@raises(ArgsParseFailure)
def test_argparser_md5_and_reviewid_exclusive():
    """
    Test mutual exclusivity of --md5 and --review-id is enforced
    """
    get_parser(SysFake()).parse_args(
        [ "-m"
        , "a93bcc098674a50ea93791fc528bdd9f"
        , "-r"
        , "SUSE:Maintenance:1:1"
        ])

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

def test_main():
    c = ConfigFake()
    trff = TestReportFactoryFake()
    pf = OneShotFactory(PromptFake)
    lf = LogFake()
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
    lf = LogFake()

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
