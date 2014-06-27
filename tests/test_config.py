# -*- coding: utf-8 -*-

from nose.tools import eq_
from nose.tools import ok_
from .utils import ConfigFake

class ArgsFake(object):
    def __init__(self):
        self.location = None
        self.template_dir = None
        self.connection_timeout = None

def test_merge_args():
    af = ArgsFake()
    af.location = 'prague'
    c = ConfigFake()

    ok_(af.location != c.location, "tautological setup")
    c.merge_args(af)
    eq_(af.location, c.location)
