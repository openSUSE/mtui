"""Tests for ``mtui.template.pitestreport.PITestReport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mtui.test_reports.pi_report import PITestReport
from mtui.types import RequestReviewID


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.reports_url = "https://reports.example/"
    cfg.fancy_reports_url = "https://fancy.example/"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    return cfg


def test_pi_id_returns_rrid_str(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    assert r.id == "SUSE:PI:42:99"


def test_pi_parser_returns_hosts_and_json(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    assert set(r._parser()) == {"hosts", "json"}


def test_pi_show_yourself_data_includes_repo_rows(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    r.repositories = frozenset(["repoA", "repoB"])
    rows = r._show_yourself_data()
    repo_values = [v for k, v in rows if k == "Repo"]
    assert set(repo_values) == {"repoA", "repoB"}


def test_pi_set_repo_add_uses_ar_form(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "add")  # ty: ignore[invalid-argument-type]
    args, _ = target.repo_manager.run_zypper.call_args
    assert "ar" in args[0]


def test_pi_set_repo_remove(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "remove")  # ty: ignore[invalid-argument-type]
    args, _ = target.repo_manager.run_zypper.call_args
    assert args[0] == "-n rr"


def test_pi_set_repo_unknown_raises(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    r.update_repos = {}
    with pytest.raises(ValueError, match="Not supported"):
        r.set_repo(MagicMock(), "nope")  # ty: ignore[invalid-argument-type]


def test_pi_check_hash_always_true(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    assert r.check_hash() == (True, "", "")


def test_pi_list_update_commands_invokes_display(tmp_path: Path) -> None:
    r = PITestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:PI:42:99")
    r.packages = {"v1": {"bash"}}
    display = MagicMock()
    target = MagicMock()
    target.doer.return_value = {"command": MagicMock()}
    target.doer.return_value["command"].safe_substitute.return_value = "zypper in bash"
    r.list_update_commands({"h1": target}, display)  # ty: ignore[invalid-argument-type]
    display.assert_called_once()
