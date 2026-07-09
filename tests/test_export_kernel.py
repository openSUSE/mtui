"""Tests for ``mtui.update_workflow.export.kernel.KernelExport``."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

from mtui.update_workflow.export.kernel import KernelExport


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


def _make(tmp_path: Path, template: list[str] | None = None) -> KernelExport:
    cfg = _config(tmp_path)
    openqa = MagicMock()
    openqa.kernel = []
    tpl: list[str] = template if template is not None else [""]
    return KernelExport(
        cfg,
        openqa,
        tpl,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
    )


# ---------------------------------------------------------------------------
# kernel_results
# ---------------------------------------------------------------------------


def test_kernel_results_inserts_results_into_regression_section(
    tmp_path: Path,
) -> None:
    template = [
        "regression tests:\n",
        "(put your details here)\n",
        "build log review:\n",
    ]
    exporter = _make(tmp_path, template=template)
    fake_result = MagicMock()
    fake_result.pp = ["Result line 1\n"]
    exporter.openqa.kernel = [fake_result]
    exporter.kernel_results()
    joined = "".join(exporter.template)
    assert "Results from openQA" in joined
    assert "Result line 1" in joined


def test_reexport_keeps_openqa_overview_block(tmp_path: Path) -> None:
    """The overview block must survive a re-export.

    From the second export on, kernel_results' placeholder-absent branch
    bulk-deletes the regression-section body up to "build log review:" to
    drop the previous run's results. inject_overview used to run first in
    run(), so the freshly injected marker-bounded openqa_overview block sat
    exactly in that range and was deleted again on every export after the
    first -- the report silently lost its overview data.

    Runs the real run() pipeline (template-mutating stages live, the
    network/filesystem stages patched out) twice, like a tester exporting,
    refreshing results, and exporting again.
    """
    from mtui.data_sources.oqa_search import VersionResult

    template = [
        "regression tests:\n",
        "(put your details here)\n",
        "\n",
        "build log review:\n",
    ]
    exporter = _make(tmp_path, template=template)
    fake_result = MagicMock()
    fake_result.pp = ["Result line 1\n"]
    exporter.openqa.kernel = [fake_result]
    exporter.openqa.auto = None  # inject_openqa: nothing to inject
    overview = MagicMock()
    overview.single_incidents = [
        VersionResult(version="15-SP6", url="https://oqa/t1", status="passed")
    ]
    overview.aggregated_updates = []
    overview.build_checks = []
    overview.skip_aggregated = True
    exporter.openqa.overview = overview

    def _export_once() -> str:
        with (
            patch.object(exporter, "install_results"),
            patch.object(exporter, "get_logs", return_value=[]),
            patch.object(exporter, "installlogs_lines"),
            patch.object(exporter, "add_sysinfo"),
            patch.object(exporter, "dedup_lines"),
        ):
            exporter.run()
        return "".join(exporter.template)

    first = _export_once()
    assert "openqa_overview begin" in first
    assert "15-SP6" in first

    second = _export_once()
    # Exactly one overview block and one results run survive the re-export.
    assert second.count("openqa_overview begin") == 1
    assert "15-SP6" in second
    assert second.count("Results from openQA:\n") == 1


# ---------------------------------------------------------------------------
# get_logs
# ---------------------------------------------------------------------------


def test_get_logs_writes_into_install_logs_dir(tmp_path: Path) -> None:
    exporter = _make(tmp_path)
    # Create a fake log file in the expected directory.
    in_path = (
        exporter.config.template_dir / str(exporter.rrid) / exporter.config.install_logs
    )
    in_path.mkdir(parents=True, exist_ok=True)
    (in_path / "foo.log").write_text("data")
    with (
        patch("mtui.update_workflow.export.kernel.download_logs") as dl,
        patch(
            "mtui.update_workflow.export.kernel.ensure_dir_exists",
            side_effect=lambda p: p,
        ),
    ):
        out = exporter.get_logs()
    dl.assert_called_once()
    assert "foo.log" in out


# ---------------------------------------------------------------------------
# run pipeline
# ---------------------------------------------------------------------------


def test_run_invokes_pipeline(tmp_path: Path) -> None:
    exporter = _make(tmp_path)
    with (
        patch.object(exporter, "install_results") as install_results,
        patch.object(exporter, "inject_openqa") as inject_openqa,
        patch.object(exporter, "inject_overview") as inject_overview,
        patch.object(exporter, "kernel_results") as kernel_results,
        patch.object(exporter, "get_logs", return_value=["x.log"]) as get_logs,
        patch.object(exporter, "installlogs_lines") as installlogs_lines,
        patch.object(exporter, "add_sysinfo") as add_sysinfo,
        patch.object(exporter, "dedup_lines") as dedup_lines,
    ):
        out = exporter.run()
    install_results.assert_called_once()
    inject_openqa.assert_called_once()
    inject_overview.assert_called_once()
    kernel_results.assert_called_once()
    get_logs.assert_called_once()
    installlogs_lines.assert_called_once_with(["x.log"])
    add_sysinfo.assert_called_once()
    dedup_lines.assert_called_once()
    assert out is exporter.template
