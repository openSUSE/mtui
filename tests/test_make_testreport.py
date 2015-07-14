# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function
from __future__ import absolute_import

import os
import shutil
from os.path import dirname
from tempfile import mkdtemp

from tests.utils import ConfigFake
from tests.utils import LogFake
from tests.utils import get_nonexistent_path
from tests.utils import unused

from nose.tools import ok_, eq_

from mtui.template import TestReport
from mtui.template import SwampTestReport
from mtui.template import SwampUpdateID
from mtui.template import _TemplateIOError


class TestReportSVNCheckoutFake(object):
  def __init__(self, template_path):
    self.path = template_path

  def __call__(self, *a, **kw):
    os.makedirs(dirname(self.path))
    with open(self.path, "w") as f:
      f.write("unused")


def test_UID_mtr_success():
  """
  Test UpdateID.make_testreport immediate success

  1. returns the TestReport instance from UpdateID.testreport_factory

  2. passes config and log objects on to the testreport instance
  """

  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = unused

    u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
    u.config = c
    u.log = LogFake()
    class TestReportFake(SwampTestReport):
      def _parse(self, file_):
        pass

    u.testreport_factory = TestReportFake

    TestReportSVNCheckoutFake(u._template_path())()

    tr = u.make_testreport()

    ok_(isinstance(tr, TestReportFake))
    eq_(u.config, tr.config)
    eq_(u.log, tr.log)
  finally:
    shutil.rmtree(d)


def test_UID_mtr_with_checkout():
  """
  Test UpdateID.make_testreport does vcs_checkout if can't read the
  report
  """
  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'

    u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
    u.config = c
    u.log = LogFake()
    u._vcs_checkout = TestReportSVNCheckoutFake(u._template_path())

    class TestReportFake(SwampTestReport):
      def _parse(self, file_):
        pass
    u.testreport_factory = TestReportFake

    tr = u.make_testreport()

    ok_(isinstance(tr, TestReportFake))
    eq_(u.config, tr.config)
    eq_(u.log, tr.log)
  finally:
    shutil.rmtree(d)


def test_UID_mtr_failing_checkout():
  """
  Test UpdateID.make_testreport raises
  """
  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'

    u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
    u.config = c
    u.log = LogFake()
    u._vcs_checkout = lambda *a, **kw: unused

    tr = u.make_testreport()
  except _TemplateIOError:
    pass
  else:
    ok_(False, "_TemplateIOError expected to be raised")
  finally:
    shutil.rmtree(d)


def test_UID_mtr_other_ioerror():
  """
  Test UpdateID.make_testreport raises
  """

  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'

    u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
    u.config = c
    u.log = LogFake()
    u._vcs_checkout = lambda *a, **kw: \
      ok_(False, "shouldn't try to perform checkout")

    TestReportSVNCheckoutFake(u._template_path())()
    os.chmod(u._template_path(), 0)

    try:
      tr = u.make_testreport()
    except _TemplateIOError as e:
      pass
    else:
      ok_(False, "_TemplateIOError expected to be raised")
  finally:
    shutil.rmtree(d)


def test_UID_mtr__copy_scripts_src_missing():
  """
  Test make_testreport() when scripts are missing in datadir
  """

  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'
    c.datadir = get_nonexistent_path()

    u = SwampUpdateID('82407e2d7113cfde72f65d81e4ffee61')
    u.config = c
    u.log = LogFake()
    u._vcs_checkout = TestReportSVNCheckoutFake(u._template_path())

    tr = u.make_testreport()
    eq_(tr.log.errors[-1], 'copy scripts manually')
    ok_(isinstance(tr, TestReport))
  finally:
    shutil.rmtree(d)

