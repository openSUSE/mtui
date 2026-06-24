"""Tests for the `list_templates` command."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.templates import ListTemplates
from mtui.template_registry import TemplateRegistry
from mtui.types import Workflow


def _report(rrid, *, hosts=0, workflow=Workflow.MANUAL):
    report = MagicMock()
    report.id = rrid
    report.workflow = workflow
    report.targets = {f"h{i}": MagicMock() for i in range(hosts)}
    return report


def _null():
    null = MagicMock()
    null.id = ""
    null.targets = {}
    return null


def _registry(*reports):
    reg = TemplateRegistry(MagicMock(), null_factory=_null)
    for r in reports:
        reg.add(r)
    return reg


def _prompt(registry):
    p = MagicMock()
    p.templates = registry
    return p


def _run(prompt, mock_config):
    buf = io.StringIO()
    sys_mock = MagicMock()
    sys_mock.stdout = buf
    cmd = ListTemplates(Namespace(), mock_config, sys_mock, prompt)
    cmd()
    return buf.getvalue().splitlines()


def test_list_templates_empty(mock_config):
    prompt = _prompt(_registry())
    lines = _run(prompt, mock_config)
    assert lines == ["No templates loaded."]


def test_list_templates_marks_active(mock_config):
    r1 = _report("SUSE:Maintenance:1:1", hosts=2, workflow=Workflow.AUTO)
    r2 = _report("SUSE:Maintenance:2:2", hosts=1, workflow=Workflow.KERNEL)
    reg = _registry(r1, r2)
    reg.set_active("SUSE:Maintenance:2:2")
    lines = _run(_prompt(reg), mock_config)

    assert len(lines) == 2
    assert lines[0].startswith("  SUSE:Maintenance:1:1")
    assert "hosts: 2" in lines[0]
    assert "mode: auto" in lines[0]
    assert lines[1].startswith("* SUSE:Maintenance:2:2")
    assert "hosts: 1" in lines[1]
    assert "mode: kernel" in lines[1]
