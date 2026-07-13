"""Tests for :mod:`mtui.cli._history`."""

from __future__ import annotations

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
