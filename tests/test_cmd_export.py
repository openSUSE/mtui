"""Tests for the manual-branch of the `export` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.export import Export
from mtui.support.messages import TestReportNotLoadedError


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.path = "/tmp/log"
    p.metadata.openqa = MagicMock()
    p.metadata.openqa.auto = MagicMock()  # truthy -> skips DashboardAutoOpenQA branch
    p.metadata.report_results.return_value = []
    p.display = MagicMock()
    p.targets = MagicMock()
    p.targets.select.return_value = MagicMock()
    p.targets.select.return_value.values.return_value = []
    p.targets.select.return_value.keys.return_value = []
    return p


def _filelist_ctx() -> MagicMock:
    ctx = MagicMock()
    ctx.__enter__.return_value = []
    ctx.__exit__.return_value = None
    return ctx


def test_export_manual_branch_instantiates_manual_exporter(mock_config):
    prompt = _prompt()
    mock_config.auto = False
    mock_config.kernel = False
    args = Namespace(filename=None, hosts=None, force=False)

    # ManualExport must be a real class so the `issubclass()` check inside
    # `Export.__call__` returns True. Subclass the real one and stub `run`.
    from mtui.export.manual import ManualExport as RealManual

    class StubManual(RealManual):
        def __init__(self, *a, **kw):  # noqa: D401 - test stub
            self.called_with = (a, kw)

        def run(self, *_a, **_k):  # type: ignore[override]
            return []

    with (
        patch("mtui.commands.export.ManualExport", StubManual),
        patch("mtui.commands.export.FileList") as fl,
    ):
        fl.load.return_value = _filelist_ctx()
        Export(args, mock_config, MagicMock(), prompt)()
        # No exception means the manual branch ran cleanly.


def test_export_logs_exception_on_filelist_failure(mock_config, caplog):
    """Inner-block exceptions are caught and `logger.exception` logs them."""
    prompt = _prompt()
    mock_config.auto = False
    mock_config.kernel = False
    args = Namespace(filename=Path("/nonexistent"), hosts=None, force=False)
    caplog.set_level(logging.ERROR, logger="mtui.commands.export")

    from mtui.export.manual import ManualExport as RealManual

    class StubManual(RealManual):
        def __init__(self, *a, **kw):
            pass

        def run(self, *_a, **_k):  # type: ignore[override]
            raise RuntimeError("kaboom")

    with (
        patch("mtui.commands.export.ManualExport", StubManual),
        patch("mtui.commands.export.FileList") as fl,
    ):
        fl.load.return_value = _filelist_ctx()
        Export(args, mock_config, MagicMock(), prompt)()

    assert any(
        "While exporting template was thrown exception" in r.message
        for r in caplog.records
    )


def test_export_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        Export(
            Namespace(filename=None, hosts=None, force=False),
            mock_config,
            MagicMock(),
            prompt,
        )()
