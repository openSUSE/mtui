from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

from mtui.commands.export import Export
from mtui.export.auto import AutoExport
from mtui.export.base import BaseExport
from mtui.types import FileList, OpenQAResults, RequestReviewID, URLs


class ExportProbe(BaseExport):
    def get_logs(self, *args, **kwds):
        return []

    def run(self, *args, **kwds):
        return self.template


def test_inject_openqa_replaces_dashboard_results(mock_config):
    openqa = MagicMock()
    openqa.__bool__.return_value = True
    openqa.pp = ["Results from openQA jobs:\n", "new result\n"]
    template = FileList(
        [
            "build log review:\n",
            "Results from openQA jobs:\n",
            "old result\n",
            "End of openQA Incidents results\n",
            "source code change review:\n",
        ]
    )

    exporter = ExportProbe(
        mock_config,
        OpenQAResults(auto=openqa),
        template,
        False,
        "SUSE:Maintenance:1:1",
        False,
    )

    exporter.inject_openqa()

    assert "old result\n" not in exporter.template
    assert "new result\n" in exporter.template


def test_manual_export_loads_dashboard_results_before_export(mock_config, tmp_path):
    filename = tmp_path / "log"
    filename.write_text("source code change review:\n")
    prompt = MagicMock()
    prompt.metadata.id = "SUSE:Maintenance:12358:199773"
    prompt.metadata.rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    prompt.metadata.incident = MagicMock()
    prompt.metadata.openqa = OpenQAResults()
    prompt.metadata.path = filename
    prompt.metadata.report_results.return_value = []
    prompt.display = MagicMock()
    prompt.targets.select.return_value.values.return_value = []

    args = Namespace(filename=Path(filename), force=False, hosts=None)
    mock_config.auto = False
    mock_config.kernel = False

    with patch("mtui.commands.export.DashboardAutoOpenQA") as dashboard:
        dashboard.return_value.run.return_value = MagicMock()
        with patch("mtui.commands.export.FileList.load") as load:
            load.return_value.__enter__.return_value = FileList(
                ["source code change review:\n"]
            )
            load.return_value.__exit__.return_value = None
            Export(args, mock_config, MagicMock(), prompt)()

    dashboard.assert_called_once_with(
        mock_config,
        mock_config.openqa_instance,
        prompt.metadata.incident,
        prompt.metadata.rrid,
    )


def test_auto_export_replaces_stale_install_results(mock_config):
    openqa = MagicMock()
    openqa.pp = []
    openqa.results = [
        URLs(
            "sle",
            "x86_64",
            "15-SP7",
            "https://openqa.example.com/tests/1001/file/install-logs.tar",
            "passed",
        )
    ]
    template = FileList(
        [
            "Test results by product-arch:\n",
            "#############################\n",
            "\n",
            "source code change review:\n",
            "##############\n",
            "Install tests:\n",
            "##############\n",
            "\n",
            "Installation tests done in openQA with following results: FAILED\n",
            "\n",
            "sle_15-SP7_x86_64 => none: https://openqa.example.com/tests/1001\n",
            "\n",
            "Links for update logs:\n",
        ]
    )

    exporter = AutoExport(
        mock_config,
        OpenQAResults(auto=openqa),
        template,
        False,
        "SUSE:Maintenance:1:1",
        False,
    )
    with patch.object(exporter, "get_logs", return_value=[]):
        result = exporter.run()

    assert (
        "Installation tests done in openQA with following results: FAILED\n"
        not in result
    )
    assert (
        "sle_15-SP7_x86_64 => none: https://openqa.example.com/tests/1001\n"
        not in result
    )
    assert (
        "Installation tests done in openQA with following results: PASSED\n" in result
    )
    assert (
        "sle_15-SP7_x86_64 => PASSED: https://openqa.example.com/tests/1001\n" in result
    )


def _make_auto_exporter(mock_config, openqa) -> AutoExport:
    return AutoExport(
        mock_config,
        OpenQAResults(auto=openqa),
        FileList([]),
        False,
        "SUSE:Maintenance:1:1",
        False,
    )


def test_install_status_unknown_when_auto_missing(mock_config):
    """auto.results being unavailable is distinct from a FAILED outcome."""
    exporter = _make_auto_exporter(mock_config, openqa=None)

    assert exporter._install_status() == "UNKNOWN"


def test_install_status_unknown_when_results_empty(mock_config):
    openqa = MagicMock()
    openqa.results = []
    exporter = _make_auto_exporter(mock_config, openqa=openqa)

    assert exporter._install_status() == "UNKNOWN"


def test_install_status_passed_when_all_results_pass(mock_config):
    openqa = MagicMock()
    openqa.results = [
        URLs("sle", "x86_64", "15-SP7", "https://o/tests/1/file/x", "passed"),
        URLs("sle", "x86_64", "15-SP7", "https://o/tests/2/file/x", "softfailed"),
    ]
    exporter = _make_auto_exporter(mock_config, openqa=openqa)

    assert exporter._install_status() == "PASSED"


def test_install_status_failed_when_any_result_fails(mock_config):
    openqa = MagicMock()
    openqa.results = [
        URLs("sle", "x86_64", "15-SP7", "https://o/tests/1/file/x", "passed"),
        URLs("sle", "x86_64", "15-SP7", "https://o/tests/2/file/x", "failed"),
    ]
    exporter = _make_auto_exporter(mock_config, openqa=openqa)

    assert exporter._install_status() == "FAILED"


# --- inject_overview ---


def test_inject_overview_writes_block_into_template(mock_config):
    """BaseExport.inject_overview inserts the marker-bounded block."""
    from mtui.connector.oqa_search import (
        OVERVIEW_BEGIN_MARKER,
        OVERVIEW_END_MARKER,
        VersionResult,
    )
    from mtui.types import OpenQAOverviewResult

    template = FileList(
        [
            "regression tests:\n",
            "-----------------\n",
            "\n",
            "(put your details here)\n",
            "\n",
            "build log review:\n",
            "-----------------\n",
        ]
    )
    overview = OpenQAOverviewResult(
        single_incidents=[VersionResult("15-SP5", "u1", "passed")]
    )

    exporter = ExportProbe(
        mock_config,
        OpenQAResults(overview=overview),
        template,
        False,
        "SUSE:Maintenance:1:1",
        False,
    )

    exporter.inject_overview()

    body = "".join(exporter.template)
    assert OVERVIEW_BEGIN_MARKER in body
    assert OVERVIEW_END_MARKER in body
    assert "15-SP5" in body


def test_inject_overview_is_noop_when_overview_unset(mock_config):
    """No overview means no template mutation."""
    template = FileList(
        [
            "regression tests:\n",
            "-----------------\n",
            "(put your details here)\n",
            "build log review:\n",
        ]
    )
    original = list(template)

    exporter = ExportProbe(
        mock_config,
        OpenQAResults(),
        template,
        False,
        "SUSE:Maintenance:1:1",
        False,
    )

    exporter.inject_overview()

    assert list(exporter.template) == original
