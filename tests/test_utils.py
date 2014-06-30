# -*- coding: utf-8 -*-

from nose.tools import ok_, eq_, raises
from unittest import TestCase

import os
import pwd
import shutil
from tempfile import mkdtemp
from os.path import join

from mtui.utils import ensure_dir_exists
from mtui.utils import chdir
from mtui.xdg import save_cache_path

class TestEnsureDirExists(TestCase):
    def setUp(self):
        self.root = mkdtemp()
        self._callback_paths = []

    def test_create(self):
        d = self.mkpath('a')
        ensure_dir_exists(d)

    def test_create_exists(self):
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
