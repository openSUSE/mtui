# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_
from nose.tools import raises

from mtui.main import get_parser
from mtui.main import run_mtui
from .utils import ConfigFake
from .utils import SysFake
from .utils import OneShotFactory
from .utils import LogFake
from .utils import rand_maintenance_id
from .utils import rand_review_id
from mtui.argparse import ArgsParseFailure
from mtui.types.obs import RequestReviewID

import  unittest

# TODO: check the args get passed correctly into the application once
# the main() was refactored enough

def test_argparser_sut1():
    p = get_parser(SysFake())
    a = p.parse_args(["-s", "foo,bar"])
    eq_(a.sut[0].print_args(), '-s bar -t foo')


def test_argparser_sut2():
    p = get_parser(SysFake())
    a = p.parse_args(["--sut", "bar,foo"])
    eq_(a.sut[0].print_args(),'-s foo -t bar')

def test_argparser_sut_multi():
    p = get_parser(SysFake())
    a = p.parse_args(["--sut", "bar,foo,doo"])
    eq_(a.sut[0].print_args(),'-s doo -t bar -t foo')

def test_argparser_sut_multi_split():
    p = get_parser(SysFake())
    a = p.parse_args(["--sut", "bar,doo","-s","foo,doo"])
    eq_(a.sut[0].print_args(), "-s doo -t bar")
    eq_(a.sut[1].print_args(), "-s doo -t foo")

@raises(ArgsParseFailure)
def test_argparser_sut_fail():
    p = get_parser(SysFake())
    p.parse_args(["--sut", "doo"])

def helper_parse_reviewid(rrid):
    return get_parser(SysFake()).parse_args(
        [ "-r"
        , rrid
        ])

def test_argparser_reviewid_ok():
    """
    Test correct RRID is parsed successfully
    """

    rrid = RequestReviewID("SUSE:Maintenance:{0}:{1}".format(
        rand_maintenance_id(),
        rand_review_id()
    ))

    parsed = helper_parse_reviewid(str(rrid)).review_id
    eq_(parsed.id.review_id, rrid.review_id)
    eq_(parsed.id.maintenance_id, rrid.maintenance_id)

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

class PromptFake(object):
    def __init__(self, *args, **kw):
        self.t_autoadds = []
        self.t_cmdloops = 0
        self.t_cmdqueues = []

    def do_autoadd(self, line):
        self.autoadds.append(line)

    def cmdloop(self):
        self.t_cmdloops += 1

    def set_cmdqueueu(self, queue):
        self.t_cmdqueues.append(queue)

def test_main():
    """
    Test main happy path without args gets to running the prompt cmdloop
    """
    pf = OneShotFactory(PromptFake)
    a = get_parser(SysFake()).parse_args([])
    ok_(run_mtui(SysFake(["mtui"]), ConfigFake(), LogFake(), pf, None, a) is 0)

    eq_(pf.product.t_cmdloops, 1)

def test_main_config_overrides():
    """
    Test argv options override their config counterparts
    """
    location = 'foolocation'
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

    sysf = SysFake([
      "mtui"
    , "-l", location
    , "-t", template_dir
    , "-w", timeout
    ])
    a = get_parser(sysf).parse_args(["-l", location, "-t", template_dir, "-w", timeout])

    ok_(run_mtui(sysf, c, LogFake(), PromptFake, None, a) is 0)

    for x, y in overrides:
        eq_(x, y(), "override didn't take effect: {0} != {1}".format(x, y()))
