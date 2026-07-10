"""Tests for the helpers in :mod:`mtui.fileops`."""

import os
from pathlib import Path
from tempfile import mkdtemp
from typing import ClassVar
from unittest.mock import patch

import pytest

from mtui.support.fileops import atomic_write_file, chdir, ensure_dir_exists, timestamp


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
        try:
            with pytest.raises(PermissionError):
                ensure_dir_exists(Path(subdir) / "bar")
        finally:
            # Restore write/exec so pytest's tmpdir teardown can remove subdir.
            # Without this the mode-0 directory is undeletable; pytest renames it
            # to garbage-<uuid> and the leftovers pile up under
            # /tmp/pytest-of-<user>/ across runs, emitting an rm_rf warning each
            # time it retries.
            os.chmod(subdir, 0o755)

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


def test_atomic_write_non_ascii_is_utf8_on_disk(create_temp):
    """The write encoding matches the UTF-8 read path, byte for byte."""
    target = Path(create_temp) / "utf8.txt"
    atomic_write_file("Bjørn — テスト", target)
    assert target.read_bytes() == "Bjørn — テスト".encode()


def test_atomic_write_non_ascii_survives_non_utf8_locale(create_temp):
    """Writing non-ASCII must not depend on the process locale.

    fdopen without an explicit encoding uses the locale codec, while the
    read path (FileList.load, downloaded payloads) is UTF-8. Under a
    non-UTF-8 locale (LC_ALL=C with PEP 538/540 coercion disabled) a
    template containing 'Bjørn' died with UnicodeEncodeError, losing the
    edits. Reproduced in a subprocess because the interpreter's locale
    and UTF-8 mode are fixed at startup.
    """
    import subprocess
    import sys

    target = Path(create_temp) / "out.txt"
    code = (
        "from pathlib import Path\n"
        "from mtui.support.fileops import atomic_write_file\n"
        f"atomic_write_file('Bj\\u00f8rn', Path({str(target)!r}))\n"
    )
    env = dict(os.environ)
    env.update(LC_ALL="C", LANG="C", PYTHONUTF8="0", PYTHONCOERCECLOCALE="0")
    proc = subprocess.run(
        [sys.executable, "-c", code],
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,  # the assertion below reports stderr on failure
    )
    assert proc.returncode == 0, proc.stderr
    assert target.read_bytes() == "Bjørn".encode()


def test_atomic_write_creates_missing_parent(create_temp):
    """The destination directory is created if absent (e.g. ~/.cache/mtui)."""
    target = Path(create_temp) / "missing" / "nested" / "refhosts.yml"
    assert not target.parent.exists()
    atomic_write_file(b"data: 1\n", target)
    assert target.exists()
    assert target.read_text() == "data: 1\n"


def test_atomic_write_cleans_temp_on_move_failure(create_temp):
    """A failed move must not leave the mkstemp temp file behind."""
    target = Path(create_temp) / "out"
    before = set(os.listdir(create_temp))

    with (
        patch("mtui.support.fileops.move", side_effect=OSError("boom")),
        pytest.raises(OSError, match="boom"),
    ):
        atomic_write_file("data", target)

    # No temp file leaked into the destination directory, and no partial target.
    assert set(os.listdir(create_temp)) == before
    assert not target.exists()


def test_timestamp():
    """Test timestamp."""
    assert isinstance(int(timestamp()), int)
