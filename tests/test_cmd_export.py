"""Tests for the manual-branch of the `export` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.export import Export
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import NoRefhostsDefinedError, TestReportNotLoadedError
from mtui.types import Workflow


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.workflow = Workflow.MANUAL
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
    args = Namespace(filename=None, hosts=None, force=False)

    # ManualExport must be a real class so the `issubclass()` check inside
    # `Export.__call__` returns True. Subclass the real one and stub `run`.
    from mtui.update_workflow.export.manual import ManualExport as RealManual

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
    args = Namespace(filename=Path("/nonexistent"), hosts=None, force=False)
    caplog.set_level(logging.ERROR, logger="mtui.commands.export")

    from mtui.update_workflow.export.manual import ManualExport as RealManual

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


# --- fan-out: host-less templates gated by _requires_hosts ---


class _FakeReport:
    """Minimal TestReport stand-in carrying id, workflow and empty targets."""

    def __init__(self, rrid: str, workflow: Workflow):
        self.id = rrid
        self.workflow = workflow
        self.targets = HostsGroup([])


class _FakeRegistry:
    """Minimal TemplateRegistry stand-in for fan-out resolution."""

    def __init__(self, reports):
        self._reports = {str(r.id): r for r in reports}

    def all(self):
        return list(self._reports.values())

    def get(self, rrid):
        return self._reports[rrid]

    def __len__(self):
        return len(self._reports)

    @property
    def active(self):
        return next(iter(self._reports.values()))


def _fanout_cmd(mock_config, reports):
    """Build an Export whose __call__ only records the RRID it ran against."""
    args = Namespace(
        filename=None, hosts=None, force=False, template=None, all_templates=False
    )
    prompt = MagicMock()
    prompt.templates = _FakeRegistry(reports)
    prompt.metadata = prompt.templates.active
    prompt.targets = prompt.templates.active.targets
    prompt.display = MagicMock()
    prompt.interactive = False  # exercise the multi-template fan-out branch
    cmd = Export(args, mock_config, MagicMock(), prompt)
    return cmd


@pytest.mark.parametrize("workflow", [Workflow.AUTO, Workflow.KERNEL])
def test_requires_hosts_false_for_auto_and_kernel(mock_config, workflow):
    cmd = _fanout_cmd(mock_config, [_FakeReport("A", workflow)])
    assert cmd._requires_hosts(_FakeReport("A", workflow)) is False


def test_requires_hosts_true_for_manual(mock_config):
    cmd = _fanout_cmd(mock_config, [_FakeReport("A", Workflow.MANUAL)])
    assert cmd._requires_hosts(_FakeReport("A", Workflow.MANUAL)) is True


@pytest.mark.parametrize("workflow", [Workflow.AUTO, Workflow.KERNEL])
def test_fanout_hostless_auto_kernel_runs_without_error(mock_config, workflow):
    """AUTO/KERNEL export over all-host-less templates runs, never skips."""
    reports = [_FakeReport("A", workflow), _FakeReport("B", workflow)]
    cmd = _fanout_cmd(mock_config, reports)
    ran: list[str] = []
    with patch.object(
        Export, "__call__", lambda self: ran.append(str(self.metadata.id))
    ):
        cmd.run()  # must not raise NoRefhostsDefinedError
    assert ran == ["A", "B"]


def test_fanout_hostless_manual_still_raises(mock_config):
    """MANUAL export over all-host-less templates keeps the host-phase error."""
    reports = [_FakeReport("A", Workflow.MANUAL), _FakeReport("B", Workflow.MANUAL)]
    cmd = _fanout_cmd(mock_config, reports)
    with (
        patch.object(Export, "__call__", lambda self: None),
        pytest.raises(NoRefhostsDefinedError),
    ):
        cmd.run()
