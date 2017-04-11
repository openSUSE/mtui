# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_
from nose.tools import raises

from mtui.config import InvalidOptionNameError
import configparser
import os
from .utils import ConfigFake
from mtui.commands.config import Config

class ArgsFake(object):
    def __init__(self):
        self.location = None
        self.template_dir = None
        self.connection_timeout = None

def test_merge_args():
    af = ArgsFake()
    af.location = 'foolocation'
    c = ConfigFake()

    ok_(af.location != c.location, "tautological setup")
    c.merge_args(af)
    eq_(af.location, c.location)

@raises(InvalidOptionNameError)
def test_set_option_invalid():
    c = ConfigFake()
    c.set_option('foobar', 'kek')

def test_config_parse():
    cfg = ConfigFake()

    cfg.config = configparser.SafeConfigParser()
    cfg._define_config_options()
    cfg.configfiles=[_find_config_file('basic_config.ini')]
    cfg.config.read(cfg.configfiles)
    cfg._parse_config()

    eq_(cfg.datadir, '/test/location')
    eq_(cfg.location, 'foolocation')

    eq_(cfg.testopia_interface, 'https://test.api')
    eq_(cfg.testopia_user, 'test_user')
    eq_(cfg.testopia_pass, 'test_password')

    eq_(cfg.refhosts_resolvers, 'https')
    eq_(cfg.refhosts_https_uri, 'https://test_remote/refhosts.yml')
    eq_(cfg.refhosts_path, '/path/to/refhosts/file')

    eq_(cfg.bugzilla_url, 'https://bugzilla.test')

def _find_config_file(name):
    return os.path.join(
        os.path.dirname(__file__)
        , "fixtures"
        , "config"
        , name
    )
