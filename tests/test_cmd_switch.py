"""Tests for the `switch` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.switch import Switch
from mtui.support.messages import TemplateNotLoadedError
from mtui.template_registry import TemplateRegistry


def _report(rrid):
    report = MagicMock()
    report.id = rrid
    report.targets = {}
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


def test_switch_flips_active(mock_config):
    reg = _registry(_report("SUSE:Maintenance:1:1"), _report("SUSE:Maintenance:2:2"))
    prompt = _prompt(reg)
    args = Namespace(rrid="SUSE:Maintenance:2:2")

    Switch(args, mock_config, MagicMock(), prompt)()

    assert reg.active.id == "SUSE:Maintenance:2:2"
    prompt.set_prompt.assert_called_once_with()


def test_switch_unknown_rrid_raises(mock_config):
    reg = _registry(_report("SUSE:Maintenance:1:1"))
    prompt = _prompt(reg)
    args = Namespace(rrid="SUSE:Maintenance:9:9")

    with pytest.raises(TemplateNotLoadedError):
        Switch(args, mock_config, MagicMock(), prompt)()

    assert reg.active.id == "SUSE:Maintenance:1:1"


def test_switch_completion_lists_loaded_rrids():
    reg = _registry(_report("SUSE:Maintenance:1:1"), _report("SUSE:Maintenance:2:2"))
    state = {"templates": reg}

    out = Switch.complete(
        state, "SUSE:Maintenance:2", "switch SUSE:Maintenance:2", 0, 0
    )

    assert out == ["SUSE:Maintenance:2:2"]
