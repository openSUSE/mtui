# -*- coding: utf-8 -*-

from nose.tools import eq_

from ..utils import ConfigFake
from ..prompt import make_cp

try:
    from itertools import zip_longest
except ImportError:
    from itertools import zip_longest as zip_longest


def test_config():
    """
    Test
        > config
    shows usage
    """
    cp = make_cp()
    cp.onecmd("config")
    eq_(cp.stdout.getvalue(), "usage: config [-h] {show} ...\n")


def test_config_show():
    """
    Test
        > config show
    shows the full config file
    """

    c = ConfigFake(dict(
        # redefine options which depend on environment
        datadir='foo-data',
        template_dir='foo-template',
        local_tempdir='/tmp/mtui-unittests',
        refhosts_path='foo-refhosts',
        session_user='foo-user'
        ))

    cp = make_cp(config=c)
    cp.onecmd("config show")

    for actual, expected in zip_longest(
        [(opt, val) for x in cp.stdout.getvalue().splitlines() for opt, _,
         val in [x.partition(" = ")]],
        [("{0:<25}".format(opt),
          val)
         for(opt, val)
         in
         [("datadir", "'foo-data'"),
          ("template_dir", "'foo-template'"),
          ("local_tempdir", "'/tmp/mtui-unittests'"),
          ("session_user", "'foo-user'"),
          ("connection_timeout", "300"),
          ("svn_path", "'svn+ssh://svn@qam.suse.de/testreports'"),
          ("bugzilla_url", "'https://bugzilla.suse.com'"),
          ("reports_url", "'http://qam.suse.de/testreports'"),
          ("target_tempdir", "'/tmp'"),
          ("target_testsuitedir", "'/usr/share/qa/tools'"),
          ("testopia_interface", "'https://apibugzilla.novell.com/xmlrpc.cgi'"),
          ("testopia_user", "''"),
          ("testopia_pass", "''"),
          ("chdir_to_template_dir", "False"),
          ("refhosts_resolvers", "'https'"),
          ("refhosts_https_uri", "'https://qam.suse.de/metadata/refhosts.yml'"),
          ("refhosts_https_expiration", "43200"),
          ("refhosts_path", "'foo-refhosts'"),
          ("use_keyring", 'False'),
          ('report_bug_url',
           "'https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI'"),
          ("location", "'default'")]]):
        eq_(actual, expected)


def test_config_show_one():
    """
    Test
        > config show x
    shows the x attribute
    """

    c = ConfigFake(dict(
        datadir='foo-data'
    ))
    cp = make_cp(config=c)
    cp.onecmd("config show datadir")
    eq_(cp.stdout.getvalue(), "datadir = 'foo-data'\n")
