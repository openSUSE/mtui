# -*- coding: utf-8 -*-

from nose.tools import eq_

from mtui import commands
from mtui.config import Config
from mtui.template import TestReport
from mtui.prompt import CommandAlreadyBoundError
from .utils import StringIO
from .utils import ConfigFake
from .utils import LogMock
from .test_prompt import TestableCommandPrompt

from itertools import izip_longest

# FIXME: the command objects needs to be constructed via CommandPrompt
# due to CDC's braindead implementation. See
# L{CommandPrompt.__getattr__}

def _run_config(in_, config):
    """
    :return: str stdout of the Config command

    note the burden of setting appropriate interface_version is on the
    user.
    """
    l = LogMock()
    cp = TestableCommandPrompt([], TestReport(config, l), config, l)
    try:
        cp._add_subcommand(commands.Config)
    except CommandAlreadyBoundError:
        pass
    cp.stdout = StringIO()
    cp.onecmd(in_)
    return cp.stdout.getvalue()

def test_config():
    """
    Test
        > config
    shows usage
    """
    c = ConfigFake()
    c.interface_version = commands.Config.stable
    eq_(_run_config("config", c), "usage: config [-h] {show} ...\n")

def test_config_show():
    """
    Test
        > config show
    shows the full config file
    """

    c = ConfigFake()
    # redefine options which depend on environment
    c.datadir = 'foo-data'
    c.template_dir = 'foo-template'
    c.refhosts_xml = 'foo-refhosts'
    c.session_user = 'foo-user'
    c.interface_version = '66.6'
    for x,y in izip_longest(_run_config("config show", c).splitlines(),
        [ "datadir              = 'foo-data'"
        , "template_dir         = 'foo-template'"
        , "refhosts_xml         = 'foo-refhosts'"
        , "local_tempdir        = '/tmp'"
        , "session_user         = 'foo-user'"
        , "location             = 'default'"
        , "interface_version    = '66.6'"
        , "connection_timeout   = 300"
        , "svn_path             = 'svn+ssh://svn@qam.suse.de/testreports'"
        , "patchinfo_url        = 'http://hilbert.nue.suse.com/abuildstat/patchinfo'"
        , "bugzilla_url         = 'https://bugzilla.novell.com'"
        , "reports_url          = 'http://qam.suse.de/testreports'"
        , "repclean_path        = '/mounts/qam/rep-clean/rep-clean.sh'"
        , "target_tempdir       = '/tmp'"
        , "target_testsuitedir  = '/usr/share/qa/tools'"
        , "testopia_interface   = 'https://apibugzilla.novell.com/tr_xmlrpc.cgi'"
        , "testopia_user        = ''"
        , "testopia_pass        = ''"
        , "chdir_to_templatedir = False"
        ]):
            eq_(x,y)

def test_config_show_one():
    """
    Test
        > config show x
    shows the x attribute
    """

    c = ConfigFake()
    c.datadir = 'foo-data'
    c.interface_version = commands.Config.stable
    eq_(_run_config("config show datadir", c),
        "datadir = 'foo-data'\n")
