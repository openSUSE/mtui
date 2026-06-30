"""Tests for ``mtui.update_workflow.export.manual.ManualExport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.update_workflow.export.manual import ManualExport


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.install_logs = "install_logs"
    cfg.reports_url = "https://reports"
    cfg.distro = "SLES"
    cfg.distro_ver = "15"
    cfg.distro_kernel = "5.14"
    cfg.session_user = "tester"
    return cfg


def _make(tmp_path: Path, results: list | None = None) -> ManualExport:
    cfg = _config(tmp_path)
    openqa = MagicMock()
    template: list[str] = [""]
    return ManualExport(
        cfg,
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
        results=list(results or []),
    )


# ---------------------------------------------------------------------------
# get_logs
# ---------------------------------------------------------------------------


def test_get_logs_writes_per_host_files(tmp_path: Path) -> None:
    host = MagicMock()
    host.hostname = "h1"
    host.hostlog = []
    exporter = _make(tmp_path, results=[host])
    with patch.object(exporter, "_writer") as writer:
        out = exporter.get_logs(["h1"])
    assert out == ["h1.log"]
    writer.assert_called_once()


# ---------------------------------------------------------------------------
# _host_installog_to_template
# ---------------------------------------------------------------------------


def test_host_installog_to_template_filters_zypper_and_transactional_lines(
    tmp_path: Path,
) -> None:
    host = MagicMock()
    host.hostname = "h1"
    line1 = MagicMock()
    line1.command = "zypper in bash"
    line1.stdout = "ok"
    line2 = MagicMock()
    line2.command = "ls"
    line2.stdout = "x"
    host.hostlog = [line1, line2]
    exporter = _make(tmp_path, results=[host])
    out = exporter._host_installog_to_template("h1")
    # First line: log header. Remaining: only the zypper command line.
    assert any("zypper in bash" in line for line in out)
    assert not any("ls" in line and "zypper" not in line for line in out[1:])


def test_host_installog_to_template_unknown_host_returns_empty(
    tmp_path: Path,
) -> None:
    exporter = _make(tmp_path, results=[])
    assert exporter._host_installog_to_template("missing") == []


# ---------------------------------------------------------------------------
# run pipeline
# ---------------------------------------------------------------------------


def test_run_invokes_all_pipeline_steps(tmp_path: Path) -> None:
    exporter = _make(tmp_path)
    with (
        patch.object(exporter, "install_results") as install_results,
        patch.object(exporter, "inject_openqa") as inject_openqa,
        patch.object(exporter, "inject_overview") as inject_overview,
        patch.object(exporter, "get_logs", return_value=["x.log"]) as get_logs,
        patch.object(exporter, "installlogs_lines") as installlogs_lines,
        patch.object(exporter, "add_sysinfo") as add_sysinfo,
        patch.object(exporter, "dedup_lines") as dedup_lines,
    ):
        out = exporter.run(["h1"])
    install_results.assert_called_once()
    inject_openqa.assert_called_once()
    inject_overview.assert_called_once()
    get_logs.assert_called_once_with(["h1"])
    installlogs_lines.assert_called_once_with(["x.log"])
    add_sysinfo.assert_called_once()
    dedup_lines.assert_called_once()
    assert out is exporter.template


# ---------------------------------------------------------------------------
# _fillup_hosts_to_template: install verdict (no longer script-derived)
# ---------------------------------------------------------------------------


def _pkg(name: str, before, after) -> MagicMock:
    p = MagicMock()
    p.name = name
    p.before = before
    p.after = after
    return p


def _host(packages: dict) -> MagicMock:
    host = MagicMock()
    host.hostname = "h1"
    host.system = "system1"
    host.packages = packages
    return host


def _host_block() -> list[str]:
    return [
        "system1 (reference host: h1)\n",
        "before:\n",
        "after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
    ]


@pytest.mark.parametrize(
    ("before", "after", "expected"),
    [
        (1, 2, "=> PASSED\n"),  # version went up -> PASSED
        (2, 2, "=> FAILED\n"),  # version unchanged -> FAILED
    ],
)
def test_fillup_flips_verdict_from_package_versions(
    tmp_path: Path, before, after, expected
) -> None:
    """The verdict derives purely from the package-version check now."""
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", before, after)})])
    exporter.template = _host_block()
    exporter._fillup_hosts_to_template()
    assert expected in exporter.template
    assert "=> PASSED/FAILED\n" not in exporter.template


def test_fillup_leaves_already_set_verdict_untouched(tmp_path: Path) -> None:
    """A re-export must not re-flip an already-decided verdict (idempotent)."""
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", 1, 2)})])
    template = _host_block()
    template[4] = "=> PASSED\n"  # placeholder already resolved
    exporter.template = template
    exporter._fillup_hosts_to_template()
    # the forward scan stops at the trailing ``comment:`` line without inventing
    # or re-flipping a verdict.
    assert "=> PASSED\n" in exporter.template
    assert "=> FAILED\n" not in exporter.template
