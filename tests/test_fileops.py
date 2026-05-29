"""Tests for the helpers in :mod:`mtui.fileops`."""

import os
from pathlib import Path
from tempfile import mkdtemp
from typing import ClassVar

import pytest

from mtui.fileops import atomic_write_file, chdir, ensure_dir_exists, timestamp


@pytest.fixture
def create_temp(tmpdir_factory):
    """simple tmpdir_factory wrapper."""
    return tmpdir_factory.mktemp("fileops")


class TestEnsureDirExists:
    _callback_paths: ClassVar[list] = []

    def test_create(self, create_temp):
        d = self.mkpath(create_temp, "a")
        ensure_dir_exists(d)

    def test_create_exists(self, create_temp):
        """ensure_dir_exists is obviously supposed to be convergent so second
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
            pytest.fail("ensure_dir_exists raised unexpectedly")

        os.chmod(subdir, 0)
        with pytest.raises(PermissionError):
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


def test_timestamp():
    """Test timestamp."""
    assert isinstance(int(timestamp()), int)
