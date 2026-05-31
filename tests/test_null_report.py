"""Tests for ``mtui.template.nulltestreport.NullTestReport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

from mtui.test_reports.null_report import NullTestReport


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    return cfg


def test_null_bool_is_false(tmp_path: Path) -> None:
    assert bool(NullTestReport(_config(tmp_path))) is False


def test_null_id_empty(tmp_path: Path) -> None:
    assert NullTestReport(_config(tmp_path)).id == ""


def test_null_parser_returns_empty(tmp_path: Path) -> None:
    assert NullTestReport(_config(tmp_path))._parser() == {}


def test_null_update_repos_parser_returns_empty(tmp_path: Path) -> None:
    assert NullTestReport(_config(tmp_path))._update_repos_parser() == {}


def test_null_target_wd_returns_path_join(tmp_path: Path) -> None:
    n = NullTestReport(_config(tmp_path))
    assert n.target_wd("a", "b") == (tmp_path / "target") / "a" / "b"


def test_null_list_update_commands_noop(tmp_path: Path) -> None:
    NullTestReport(_config(tmp_path)).list_update_commands({}, MagicMock())  # ty: ignore[invalid-argument-type]


def test_null_check_hash_returns_true(tmp_path: Path) -> None:
    assert NullTestReport(_config(tmp_path)).check_hash() == (True, "", "")
