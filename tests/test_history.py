"""Tests for :mod:`mtui.cli._history`."""

from __future__ import annotations

import logging
from pathlib import Path

import pytest
from prompt_toolkit.history import FileHistory

from mtui.cli import _history


@pytest.fixture(autouse=True)
def _clear_history_cache():
    """Reset the module-level memo so each test gets a clean slate."""
    _history._cache.clear()
    yield
    _history._cache.clear()


def test_default_history_path_points_at_home_mtui_history() -> None:
    assert _history.default_history_path() == Path("~").expanduser() / ".mtui_history"


def test_get_history_returns_filehistory_for_given_path(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    hist = _history.get_history(p)
    assert isinstance(hist, FileHistory)
    # ``FileHistory.filename`` is typed as ``str | bytes | PathLike``; we
    # always pass a str on construction so compare against the resolved
    # path's string form.
    assert hist.filename == str(p.resolve())


def test_get_history_memoises_per_path(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    assert _history.get_history(p) is _history.get_history(p)


def test_get_history_isolates_distinct_paths(tmp_path: Path) -> None:
    a = tmp_path / "a"
    b = tmp_path / "b"
    assert _history.get_history(a) is not _history.get_history(b)


def test_append_string_round_trips_through_load(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    hist = _history.get_history(p)
    hist.append_string("hello")
    # Force a fresh read from disk by going through a new instance.
    _history._cache.clear()
    reloaded = _history.get_history(p)
    assert list(reloaded.load_history_strings()) == ["hello"]


def test_pop_last_entry_drops_tail_from_disk(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    hist = _history.get_history(p)
    hist.append_string("first")
    hist.append_string("second")

    popped = _history.pop_last_entry(p)
    assert popped == "second"

    # New instance reads only "first" from the rewritten file.
    _history._cache.clear()
    reloaded = _history.get_history(p)
    assert list(reloaded.load_history_strings()) == ["first"]


def test_pop_last_entry_evicts_cached_in_memory_deque(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    hist = _history.get_history(p)
    hist.append_string("first")
    hist.append_string("second")

    _history.pop_last_entry(p)

    # Same cached instance: its in-memory deque must have lost the tail.
    assert hist.get_strings() == ["first"]


def test_pop_last_entry_on_missing_file_returns_none(tmp_path: Path) -> None:
    p = tmp_path / "does-not-exist"
    assert _history.pop_last_entry(p) is None


def test_pop_last_entry_on_empty_file_returns_none(tmp_path: Path) -> None:
    p = tmp_path / ".mtui_history"
    p.touch()
    assert _history.pop_last_entry(p) is None


def test_pop_last_entry_on_unrecognised_format_logs_and_keeps_file(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """A file written by the old ``readline`` backend lacks ``\\n# `` markers.

    The function must leave it untouched rather than truncating to zero.
    """
    p = tmp_path / ".mtui_history"
    p.write_bytes(b"legacy-readline-line\nanother\n")

    with caplog.at_level(logging.DEBUG, logger="mtui.cli._history"):
        assert _history.pop_last_entry(p) is None

    assert "no FileHistory record marker" in caplog.text
    # File still on disk, untouched.
    assert p.read_bytes() == b"legacy-readline-line\nanother\n"


def test_pop_last_entry_multiline_record(tmp_path: Path) -> None:
    """``FileHistory`` stores newlines inside an entry as multiple ``+`` lines."""
    p = tmp_path / ".mtui_history"
    hist = _history.get_history(p)
    hist.append_string("plain")
    hist.append_string("line1\nline2")

    popped = _history.pop_last_entry(p)
    assert popped == "line1\nline2"

    _history._cache.clear()
    reloaded = _history.get_history(p)
    assert list(reloaded.load_history_strings()) == ["plain"]
