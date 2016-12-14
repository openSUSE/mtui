# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_

from mtui import commands
from mtui.prompt import CommandAlreadyBoundError
from mtui.template import OBSTestReport
from mtui.messages import HostIsNotConnectedError
from mtui.messages import ListPackagesAllHost
from ..utils import ConfigFake
from ..utils import LogFake
from ..utils import SysFake
from ..utils import unused
from ..test_prompt import TestableCommandPrompt

def test_list_packages_all():
    """
    Test an error and deprecation hint is logged

    when running `list_packages all`
    """
    c, l = ConfigFake(), LogFake()
    cp = TestableCommandPrompt(c, l, SysFake())
    try:
        cp._add_subcommand(commands.ListPackages)
    except CommandAlreadyBoundError:
        pass

    cp.metadata  = OBSTestReport(c, l, unused)
    cp.metadata.packages = dict(foo = '1.2')
    cp.onecmd("list_packages -t all")

    eq_(cp.log.errors, [HostIsNotConnectedError('all')])
    eq_(cp.log.infos, ["Using all hosts. Warning option 'all' is decaprated"])

def test_list_packages_unavailable_host():
    """
    Test an error is re-raised

    when running `list_packages unavailable-host`
    """
    c, l = ConfigFake(), LogFake()
    cp = TestableCommandPrompt(c, l, SysFake())
    try:
        cp._add_subcommand(commands.ListPackages)
    except CommandAlreadyBoundError:
        pass

    cp.metadata  = OBSTestReport(c, l, unused)
    cp.metadata.packages = dict(foo = '1.2')
    try:
        cp.onecmd("list_packages -t unavailable")
    except HostIsNotConnectedError as e:
        eq_(cp.log.infos, [])
        eq_(e.host, 'unavailable')
    else:
        ok_(False, "HostIsNotConnectedError exception was expected to be raised")
