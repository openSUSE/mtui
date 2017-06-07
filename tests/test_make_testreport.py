# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2




import errno
import shutil
from tempfile import mkdtemp

from tests.utils import ConfigFake
from tests.utils import LogFake
from tests.utils import unused

from nose.tools import ok_, eq_

from mtui.template import UpdateID
from mtui.template import _TemplateIOError


def test_UID_mtr_success():
  """
  Test UpdateID.make_testreport immediate success

  1. returns the TestReport instance from UpdateID.testreport_factory

  2. passes config and log objects on to the testreport instance
  """

  class TestReportFake:
    def __init__(tr, *a, **kw):
      tr.init_args = a
      tr.init_kwargs = kw
      tr.read_calls = 0
      tr.read_args = []
      tr.read_kwargs = []
    def read(tr, *a, **kw):
      tr.read_calls += 1
      tr.read_args += [a]
      tr.read_kwargs += [kw]
    def connect_targets(tr, *a, **kw):
      pass

  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = unused
    l = LogFake()

    u = UpdateID('82407e2d7113cfde72f65d81e4ffee61', TestReportFake, unused)
    tr = u.make_testreport(c, l)

    ok_(isinstance(tr, TestReportFake))
    eq_(c, tr.init_args[0])
    eq_(l, tr.init_args[1])
    eq_(tr.read_calls, 1)
    eq_(tr.read_args[0], (("%s/82407e2d7113cfde72f65d81e4ffee61/log" % d),))
    eq_(tr.read_kwargs[0], dict())
  finally:
    shutil.rmtree(d)


def test_UID_mtr_with_checkout():
  """
  Test UpdateID.make_testreport does vcs_checkout if can't read the
  report
  """

  class TestReportFake:
    def __init__(tr, *a, **kw):
      tr.read_calls = 0
      tr.init_args = a
      tr.init_kwargs = kw
    def read(tr, *a, **kw):
      tr.read_calls += 1
      if tr.read_calls == 1:
        e = _TemplateIOError()
        e.errno = errno.ENOENT
        raise e
    def connect_targets(tr, *a, **kw):
      pass

  class CheckoutFake:
    def __init__(co):
      co.calls = 0
      co.args = []
      co.kwargs = []
    def __call__(co, *a, **kw):
      co.calls += 1
      co.args += [a]
      co.kwargs += [kw]

  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'
    l = LogFake()
    co = CheckoutFake()

    u = UpdateID('82407e2d7113cfde72f65d81e4ffee61', TestReportFake, co)

    tr = u.make_testreport(c, l)

    ok_(isinstance(tr, TestReportFake))
    eq_(c, tr.init_args[0])
    eq_(l, tr.init_args[1])
    eq_(tr.read_calls, 2)
    eq_(co.calls, 1)
    eq_(co.args, [(c, l, '{!s}'.format(c.svn_path),'{!s}'.format('82407e2d7113cfde72f65d81e4ffee61'))])
  finally:
    shutil.rmtree(d)


def test_UID_mtr_failing_after_checkout():
  """
  Test UpdateID.make_testreport in face of silent checkout failure
  """

  # UpdateID gets an instance of this TestReportFake class since the test
  # needs something to interrogate after the make_testreport failure.
  class TestReportFake:
    def __init__(tr):
      tr.init_calls = 0
      tr.init_args = []
      tr.init_kwargs = []
      tr.read_calls = 0
      tr.read_args = []
      tr.read_kwargs = []

    def __call__(tr, *a, **kw):
      tr.init_calls += 1
      tr.init_args = [a]
      tr.init_kwargs = [kw]
      return tr

    def read(tr, *a, **kw):
      tr.read_calls += 1
      tr.read_args += [a]
      tr.read_kwargs += [kw]
      e = _TemplateIOError()
      e.errno = errno.ENOENT
      raise e


  class CheckoutFake:
    def __init__(co):
      co.calls = 0
      co.args = []
      co.kwargs = []
    def __call__(co, *a, **kw):
      co.calls += 1
      co.args += [a]
      co.kwargs += [kw]

  tr = TestReportFake()
  co = CheckoutFake()
  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'
    l = LogFake()

    u = UpdateID('82407e2d7113cfde72f65d81e4ffee61', tr, co)

    tr = u.make_testreport(c, l)
  except _TemplateIOError as e:
    eq_(e.errno, errno.ENOENT)
    eq_(co.calls, 1)
    eq_(co.args[0], (c, l, 'svnpath', '82407e2d7113cfde72f65d81e4ffee61'))
    eq_(tr.init_calls, 1)
    eq_(tr.init_args, [(c, l)])
    eq_(tr.read_calls, 2)
    for i in (0, 1):
      eq_(tr.read_args[i], (('%s/82407e2d7113cfde72f65d81e4ffee61/log' % d),))
  else:
    ok_(False, "_TemplateIOError expected to be raised")
  finally:
    shutil.rmtree(d)


def test_UID_mtr_other_ioerror():
  """
  Test UpdateID.make_testreport raises
  """
  # UpdateID gets an instance of this TestReportFake class since the test
  # needs something to interrogate after the make_testreport failure.
  class TestReportFake:
    def __init__(tr):
      tr.init_calls = 0
      tr.init_args = []
      tr.init_kwargs = []
      tr.read_calls = 0
      tr.read_args = []
      tr.read_kwargs = []

    def __call__(tr, *a, **kw):
      tr.init_calls += 1
      tr.init_args = [a]
      tr.init_kwargs = [kw]
      return tr

    def read(tr, *a, **kw):
      tr.read_calls += 1
      tr.read_args += [a]
      tr.read_kwargs += [kw]
      if tr.read_calls == 1:
        e = _TemplateIOError()
        e.errno = errno.EACCES
        raise e

  def checkout(*a, **kw):
    ok_(False, "shouldn't try to perform checkout")

  tr = TestReportFake()
  d = mkdtemp()

  try:
    c = ConfigFake()
    c.template_dir = d
    c.svn_path = 'svnpath'
    l = LogFake()

    u = UpdateID('82407e2d7113cfde72f65d81e4ffee61', tr, checkout)

    try:
      tr = u.make_testreport(c, l)
    except _TemplateIOError as e:
      eq_(e.errno, errno.EACCES)
      eq_(tr.init_calls, 1)
      eq_(tr.init_args, [(c, l)])
      eq_(tr.read_calls, 1)
      eq_(tr.read_args, [(('%s/82407e2d7113cfde72f65d81e4ffee61/log' % d),)])
    else:
      ok_(False, "_TemplateIOError expected to be raised")
  finally:
    shutil.rmtree(d)

