"""Tests for the `load_template` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.loadtemplate import LoadTemplate
from mtui.support.messages import TestReportNotLoadedError


def _prompt(metadata_truthy: bool = False) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: metadata_truthy
    p.display = MagicMock()
    p.targets = {}
    return p


class _FakeReport:
    """Minimal loaded-template stand-in carrying an id and targets."""

    def __init__(self, rrid: str) -> None:
        self.id = rrid
        self.targets: dict = {}


class _FakeRegistry:
    """Minimal TemplateRegistry stand-in for fan-out resolution."""

    def __init__(self, reports: list[_FakeReport]) -> None:
        self._reports = {str(r.id): r for r in reports}
        self._active = reports[0]

    def all(self) -> list[_FakeReport]:
        return list(self._reports.values())

    def get(self, rrid: str) -> _FakeReport:
        return self._reports[rrid]

    def __len__(self) -> int:
        return len(self._reports)

    @property
    def active(self) -> _FakeReport:
        return self._active


def test_load_template_scope_is_single():
    """load_template must never fan out — it names its own RRID via -a/-k."""
    assert LoadTemplate.scope == "single"


def test_load_template_no_fanout_under_mcp_many_loaded(mock_config):
    """Under MCP with several templates loaded, an unscoped load runs once.

    ``scope = "single"`` keeps ``_resolve_templates`` from fanning out: a
    regression to ``"active"`` would, under MCP (non-interactive) with >1
    template loaded, re-run the load — and its host autoconnect — once per
    loaded template. Driving through ``run()`` (not ``__call__``) exercises
    that resolution path.
    """
    reg = _FakeRegistry([_FakeReport("A"), _FakeReport("B"), _FakeReport("C")])
    prompt = MagicMock()
    prompt.templates = reg
    prompt.metadata = reg.active
    prompt.targets = reg.active.targets
    prompt.display = MagicMock()
    prompt.interactive = False  # MCP: "active" would otherwise fan out
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update, template=None, all_templates=False)

    LoadTemplate(args, mock_config, MagicMock(), prompt).run()

    prompt.load_update.assert_called_once_with(update, autoconnect=True)


def test_load_template_auto_kind_loads(mock_config):
    """kind=auto -> prompt.load_update is called (template is added).

    Workflow mode (auto/kernel) is now seeded onto the TestReport by
    ``make_testreport`` during ``load_update``, not by this command.
    """
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    prompt.load_update.assert_called_once_with(update, autoconnect=True)


def test_load_template_unknown_kind_raises(mock_config):
    prompt = _prompt(metadata_truthy=False)
    update = MagicMock()
    update.kind = "unknown"
    args = Namespace(update=update)

    with pytest.raises(TestReportNotLoadedError):
        LoadTemplate(args, mock_config, MagicMock(), prompt)()


def test_load_template_does_not_close_existing_hosts(mock_config):
    """Loading a second template must not tear down the active template's hosts.

    With multi-template support ``load_template`` adds rather than overwrites,
    so it never closes the previously active connections.
    """
    existing = MagicMock()
    prompt = _prompt(metadata_truthy=True)
    prompt.targets = {"h1": existing}
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    existing.close.assert_not_called()
    prompt.load_update.assert_called_once_with(update, autoconnect=True)


def test_load_template_never_carries_over_other_templates_hosts(mock_config):
    """Loading a template must not reconnect hosts owned by another template.

    Each loaded template owns its own hosts: the previously active template's
    connected hosts are left on that template and never re-added to the newly
    loaded one.
    """
    prompt = _prompt(metadata_truthy=True)
    prompt.targets = {"h1": MagicMock(), "h2": MagicMock()}
    update = MagicMock()
    update.kind = "auto"
    args = Namespace(update=update)

    LoadTemplate(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.add_target.assert_not_called()
