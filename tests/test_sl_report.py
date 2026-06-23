"""Tests for ``mtui.test_reports.sl_report.SLTestReport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.test_reports.sl_report import SLTestReport
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


def _make_rrid_with_maint(maintenance_id: str) -> RequestReviewID:
    """Build a real RRID and patch maintenance_id."""
    r = RequestReviewID("SUSE:SLFO:99:7")
    r.maintenance_id = maintenance_id
    return r


def test_sl_id_returns_rrid_str(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    assert r.id == "SUSE:SLFO:1.1:7"


def test_sl_parser_returns_hosts_and_json(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    assert set(r._parser()) == {"hosts", "json"}


def test_sl_show_yourself_data_includes_repo_rows_and_giteapr(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.repositories = frozenset(["repoA"])
    r.giteapr = "https://gitea/pr/1"
    rows = r._show_yourself_data()
    keys = [k for k, _ in rows]
    assert "Gitea PR" in keys
    assert "Repo" in keys


def test_sl_set_repo_add(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "add")
    args, _ = target.repo_manager.run_zypper.call_args
    assert "ar" in args[0]


def test_sl_set_repo_remove(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.update_repos = {}
    target = MagicMock()
    r.set_repo(target, "remove")
    args, _ = target.repo_manager.run_zypper.call_args
    assert args[0] == "-n rr"


def test_sl_set_repo_unknown_raises(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.update_repos = {}
    with pytest.raises(ValueError, match="Not supported"):
        r.set_repo(MagicMock(), "x")


def test_sl_check_hash_maintenance_id_1_1_returns_true(tmp_path: Path) -> None:
    """When ``maintenance_id == "1.1"`` the check bypasses Gitea."""
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    assert r.check_hash() == (True, "", "")


def test_sl_check_hash_gitea_compare_match(tmp_path: Path) -> None:
    """When Gitea returns the same hash, ``check_hash`` reports a match."""
    r = SLTestReport(_config(tmp_path))
    r.rrid = _make_rrid_with_maint("2.0")
    r.giteacohash = "abc"
    r.giteaprapi = "https://gitea/api/pr/1"

    fake_gitea = MagicMock()
    fake_gitea.get_hash.return_value = "abc"
    with patch("mtui.test_reports.sl_report.Gitea", return_value=fake_gitea):
        ok, old, new = r.check_hash()
    assert ok is True
    assert old == "abc"
    assert new == "abc"


def test_sl_check_hash_gitea_compare_mismatch(tmp_path: Path) -> None:
    """When Gitea returns a different hash, ``check_hash`` reports a mismatch."""
    r = SLTestReport(_config(tmp_path))
    r.rrid = _make_rrid_with_maint("2.0")
    r.giteacohash = "abc"
    r.giteaprapi = "https://gitea/api/pr/1"

    fake_gitea = MagicMock()
    fake_gitea.get_hash.return_value = "xyz"
    with patch("mtui.test_reports.sl_report.Gitea", return_value=fake_gitea):
        ok, old, new = r.check_hash()
    assert ok is False
    assert old == "abc"
    assert new == "xyz"


def test_sl_update_repos_parser_uses_reporepoparse_when_repositories_set(
    tmp_path: Path,
) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.repositories = frozenset(["a"])
    r.products = ["p"]
    with patch(
        "mtui.test_reports.sl_report.reporepoparse", return_value={"x": "y"}
    ) as m:
        out = r._update_repos_parser()
    m.assert_called_once()
    assert out == {"x": "y"}


def test_sl_update_repos_parser_uses_slrepoparse_for_1_1(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.repository = "some-repo"
    r.products = ["p"]
    with patch("mtui.test_reports.sl_report.slrepoparse", return_value={"k": "v"}) as m:
        out = r._update_repos_parser()
    m.assert_called_once()
    assert out == {"k": "v"}


def test_sl_update_repos_parser_falls_back_to_gitrepoparse(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = _make_rrid_with_maint("2.0")
    r.repository = "some-repo"
    r.products = ["p"]
    with patch(
        "mtui.test_reports.sl_report.gitrepoparse", return_value={"g": "h"}
    ) as m:
        out = r._update_repos_parser()
    m.assert_called_once()
    assert out == {"g": "h"}


def test_sl_list_update_commands_invokes_display(tmp_path: Path) -> None:
    r = SLTestReport(_config(tmp_path))
    r.rrid = RequestReviewID("SUSE:SLFO:1.1:7")
    r.packages = {"v1": {"bash"}}
    display = MagicMock()
    target = MagicMock()
    target.doer.return_value = {"command": MagicMock()}
    target.doer.return_value["command"].safe_substitute.return_value = "zypper in bash"
    r.list_update_commands({"h1": target}, display)  # ty: ignore[invalid-argument-type]
    display.assert_called_once()
