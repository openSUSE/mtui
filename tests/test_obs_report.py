"""Tests for ``mtui.template.obstestreport.OBSTestReport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mtui.test_reports.obs_report import OBSTestReport
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


def test_obs_id_returns_rrid_str(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    assert r.id == "SUSE:Maintenance:12358:199773"
    assert "rrid" in r._attrs


def test_obs_parser_returns_hosts_and_json(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    parsers = r._parser()
    assert set(parsers) == {"hosts", "json"}


def test_obs_show_yourself_data_includes_rrid_and_rating(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    r.rating = "important"
    r.realid = "id-1"
    rows = r._show_yourself_data()
    keys = [k for k, _ in rows]
    assert "ReviewRequestID" in keys
    assert "Rating" in keys
    assert "Real ID" in keys


def test_obs_set_repo_add_invokes_zypper_add(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "add")
    target.repo_manager.run_zypper.assert_called_once()
    args, _ = target.repo_manager.run_zypper.call_args
    assert "ar" in args[0]


def test_obs_set_repo_remove_invokes_zypper_rr(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "remove")
    target.repo_manager.run_zypper.assert_called_once()
    args, _ = target.repo_manager.run_zypper.call_args
    assert args[0] == "-n rr"


def test_obs_set_repo_unknown_op_raises(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    r.update_repos = {}
    with pytest.raises(ValueError, match="Not supported"):
        r.set_repo(MagicMock(), "bogus")


def test_obs_check_hash_always_true(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    assert r.check_hash() == (True, "", "")


def test_obs_list_update_commands_invokes_display(tmp_path: Path) -> None:
    r = OBSTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    r.packages = {"v1": {"bash"}}
    display = MagicMock()
    target = MagicMock()
    target.doer.return_value = {"command": MagicMock()}
    target.doer.return_value["command"].safe_substitute.return_value = "zypper in bash"
    r.list_update_commands({"h1": target}, display)  # ty: ignore[invalid-argument-type]
    display.assert_called_once()
