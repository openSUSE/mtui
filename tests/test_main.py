# -*- coding: utf-8 -*-

from nose.tools import eq_

from mtui.main import get_parser

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
