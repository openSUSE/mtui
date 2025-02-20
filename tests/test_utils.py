from mtui.utils import ensure_dir_exists
from mtui.utils import chdir
from mtui.utils import atomic_write_file

import os

from pathlib import Path
from tempfile import mkdtemp

import pytest


@pytest.fixture(scope="function")
def create_temp(tmpdir_factory):
    """simple tmpdir_factory wrapper"""
    return tmpdir_factory.mktemp("utils")


class TestEnsureDirExists:
    _callback_paths = []

    def test_create(self, create_temp):
        d = self.mkpath(create_temp, "a")
        ensure_dir_exists(d)

    def test_create_exists(self, create_temp):
        """
        ensure_dir_exists is obviously supposed to be convergent so second
        call should result in the same state. This test asserts mainly that
        OSError(EEXIST) is not raised on second call.
        """
        d = self.mkpath(create_temp, "b", "a")
        ensure_dir_exists(d)
        ensure_dir_exists(d)

    def mkpath(self, create_temp, *p):
        a = create_temp
        return Path().joinpath(a, *p)

    def test_create_permission_denied(self, create_temp):
        root = create_temp
        subdir = mkdtemp(dir=root)

        try:
            ensure_dir_exists(Path(subdir) / "foo")
        except BaseException:
            assert False

        os.chmod(subdir, 0)
        with pytest.raises(OSError):
            ensure_dir_exists(Path(subdir) / "bar")

    def test_on_create(self, create_temp):
        d = self.mkpath(create_temp, "c")
        ensure_dir_exists(d, on_create=self._callback)
        assert self._callback_paths == [d]

    @classmethod
    def _callback(cls, path):
        cls._callback_paths.append(path)


def test_chdir(create_temp):
    oldcwd = os.getcwd()
    root = create_temp

    cwd = None
    with chdir(root):
        cwd = os.getcwd()

    assert root == cwd
    assert os.getcwd() == oldcwd


def test_atomic_write(create_temp):
    path = create_temp
    data = "pokus"
    atomic_write_file(data, Path(path) / "string")
    atomic_write_file(data.encode(), Path(path) / "bytes")


def test_sutparse():
    pass
