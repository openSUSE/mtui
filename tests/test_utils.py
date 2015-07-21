# -*- coding: utf-8 -*-

from nose.tools import ok_, eq_, raises
from unittest import TestCase

import os
import shutil
from tempfile import mkdtemp
from os.path import join

from mtui.utils import ensure_dir_exists
from mtui.utils import chdir
from mtui.utils import requires_update
from mtui.utils import ass_is
from mtui.utils import DictWithInjections
from mtui.xdg import save_cache_path
from mtui.messages import TestReportNotLoadedError

from .utils import unused
from .utils import LogFake
from .utils import LogTestingWrap

class TestEnsureDirExists(TestCase):
    def setUp(self):
        self.root = mkdtemp()
        self._callback_paths = []

    def test_create(self):
        d = self.mkpath('a')
        ensure_dir_exists(d)

    def test_create_exists(self):
        """
        ensure_dir_exists is obviously supposed to be convergent so second
        call should result in the same state. This test asserts mainly that
        OSError(EEXIST) is not raised on second call.
        """
        d = self.mkpath('b', 'a')
        ensure_dir_exists(d)
        ensure_dir_exists(d)

    def mkpath(self, *p):
        return join(self.root, *p)

    @raises(OSError)
    def test_create_permission_denied(self):
        root = mkdtemp()
        subdir = mkdtemp(dir=root)

        try:
            ensure_dir_exists(join(subdir, "foo"))
        except:
            ok_(False)

        os.chmod(subdir, 0)
        ensure_dir_exists(join(subdir, "bar"))

    def test_on_create(self):
        d = self.mkpath('c')
        ensure_dir_exists(d, on_create=self._callback)
        eq_(self._callback_paths, [d])

    def _callback(self, path):
        self._callback_paths.append(path)

    def tearDown(self):
        shutil.rmtree(self.root)

def test_chdir():
    oldcwd = os.getcwd()
    root = mkdtemp()

    cwd = None
    with chdir(root):
        cwd = os.getcwd()

    eq_(root, cwd)
    eq_(os.getcwd(), oldcwd)

def test_save_cache_path():
    p = save_cache_path("foo")
    eq_('mtui/foo', p[-8:])
    ok_(len(p) > 9)

class TestRequiresUpdate:
    class HasId(object):
        id = 69

    class PromptFake:
        def __init__(self, metadata, log):
            self.metadata = metadata
            self.log = log

        @requires_update
        def foo(self):
            pass

    def test_happy_path(self):
        p = self.PromptFake(self.HasId, LogFake())
        p.foo()
        eq_(p.log.errors,  [])

    @raises(TestReportNotLoadedError)
    def test_sad_path(self):
        p = self.PromptFake(None, unused)
        p.foo()

class Foo:
    pass

class TestAssIs:
    def test_happy(self):
        for x, y in [
            ("foo", str),
            (Foo(), Foo),
        ]:
            yield ass_is, x, y

    def test_sad(self):
        for x, y in [
            ("foo", list),
            ("foo", Foo),
            (Foo, Foo),
            (Foo(), TestAssIs),
        ]:
            yield raises(AssertionError)(ass_is), x, y

def test_dict_with_injections():
    class Foo(Exception):
        pass

    d = DictWithInjections({1: 2, 3: 4}, key_error = Foo)
    eq_(d[1], 2)
    try:
        d[4]
    except Foo:
        pass
    else:
        ok_(False, "Expected Foo to be raised")

def test_empty_logger_is_empty():
    eq_(
          LogTestingWrap(LogFake()).all()
        , dict(
              errors    = []
            , warnings  = []
            , debugs    = []
            , infos     = []
            , criticals = []
        )
    )
    eq_(LogTestingWrap().all(), LogTestingWrap.empty())

def test_LogTestingWrap():
    """
    Test messages for each level are returned for that level by all()
    """
    l = LogFake()
    l.error(1)
    l.warning(2)
    l.debug(3)
    l.info(4)
    l.critical(5)

    ltw = LogTestingWrap(LogFake()) \
        .error(1) \
        .warning(2) \
        .debug(3) \
        .info(4) \
        .critical(5)

    eq_(LogTestingWrap(l).all(), ltw.all())
    eq_(LogTestingWrap(l).all(), dict(
          errors    = [1]
        , warnings  = [2]
        , debugs    = [3]
        , infos     = [4]
        , criticals = [5]
    ))
