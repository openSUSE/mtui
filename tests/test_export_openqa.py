from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

from mtui.commands.export import Export
from mtui.export.base import BaseExport
from mtui.types import FileList, OpenQAResults, RequestReviewID


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
