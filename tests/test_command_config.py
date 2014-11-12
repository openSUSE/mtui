# -*- coding: utf-8 -*-

from nose.tools import eq_

from mtui import commands
from mtui.config import Config
from mtui.template import TestReport
from mtui.prompt import CommandAlreadyBoundError
from .utils import StringIO
from .utils import ConfigFake
from .utils import LogFake
from .utils import SysFake
from .test_prompt import TestableCommandPrompt

try:
    from itertools import zip_longest
except ImportError:
    from itertools import izip_longest as zip_longest


# FIXME: the command objects needs to be constructed via CommandPrompt
# due to CDC's braindead implementation. See
# L{CommandPrompt.__getattr__}

def _run_config(in_, config):
    """
    :return: str stdout of the Config command

    note the burden of setting appropriate interface_version is on the
    user.
    """
    cp = TestableCommandPrompt(config, LogFake(), SysFake())
    try:
        cp._add_subcommand(commands.Config)
    except CommandAlreadyBoundError:
        pass
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
    c.refhosts_path = 'foo-refhosts'
    c.session_user = 'foo-user'
    c.interface_version = '66.6'

    for actual,expected in zip_longest(
          [ (opt, val)
            for x in _run_config("config show", c).splitlines()
            for opt, _, val in [x.partition(" = ")]]
        , [("{0:<25}".format(opt), val) for (opt, val) in
            [ ("datadir"                    , "'foo-data'")
            , ("template_dir"               , "'foo-template'")
            , ("local_tempdir"              , "'/tmp'")
            , ("session_user"               , "'foo-user'")
            , ("interface_version"          , "'66.6'")
            , ("connection_timeout"         , "300")
            , ("svn_path"                   , "'svn+ssh://svn@qam.suse.de/testreports'")
            , ("bugzilla_url"               , "'https://bugzilla.novell.com'")
            , ("reports_url"                , "'http://qam.suse.de/testreports'")
            , ("repclean_path"              , "'/mounts/qam/rep-clean/rep-clean.sh'")
            , ("target_tempdir"             , "'/tmp'")
            , ("target_testsuitedir"        , "'/usr/share/qa/tools'")
            , ("testopia_interface"         , "'https://apibugzilla.novell.com/tr_xmlrpc.cgi'")
            , ("testopia_user"              , "''")
            , ("testopia_pass"              , "''")
            , ("chdir_to_template_dir"      , "False")
            , ("refhosts_resolvers"         , "'https'")
            , ("refhosts_https_uri"         , "'https://qam.suse.de/metadata/refhosts.xml'")
            , ("refhosts_https_expiration"  , "43200")
            , ("refhosts_path"              , "'foo-refhosts'")
            , ("use_keyring"                , 'False')
            , ('report_bug_url'             , "'https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI'")
            , ("location"                   , "'default'")
        ]]):
            eq_(actual,expected)

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
