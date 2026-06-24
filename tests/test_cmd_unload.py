"""Tests for the `unload` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.unload import Unload
from mtui.support.messages import TemplateNotLoadedError
from mtui.template_registry import TemplateRegistry


class _FakeTarget:
    def __init__(self):
        self.closed = False

    def close(self):
        self.closed = True


def _report(rrid, *, hosts=None):
    report = MagicMock()
    report.id = rrid
    report.targets = {name: _FakeTarget() for name in hosts or []}
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


def test_unload_drops_only_target_template(mock_config):
    r1 = _report("SUSE:Maintenance:1:1", hosts=["h1"])
    r2 = _report("SUSE:Maintenance:2:2", hosts=["h2"])
    reg = _registry(r1, r2)
    prompt = _prompt(reg)
    args = Namespace(rrid="SUSE:Maintenance:1:1")

    Unload(args, mock_config, MagicMock(), prompt)()

    assert "SUSE:Maintenance:1:1" not in reg
    assert "SUSE:Maintenance:2:2" in reg
    # only the unloaded template's host was closed
    assert r1.targets == {}
    assert r2.targets["h2"].closed is False
    prompt.set_prompt.assert_called_once_with(None)


def test_unload_active_repoints_active(mock_config):
    r1 = _report("SUSE:Maintenance:1:1")
    r2 = _report("SUSE:Maintenance:2:2")
    reg = _registry(r1, r2)  # r1 active
    prompt = _prompt(reg)
    args = Namespace(rrid="SUSE:Maintenance:1:1")

    Unload(args, mock_config, MagicMock(), prompt)()

    assert reg.active.id == "SUSE:Maintenance:2:2"


def test_unload_unknown_rrid_raises(mock_config):
    reg = _registry(_report("SUSE:Maintenance:1:1"))
    prompt = _prompt(reg)
    args = Namespace(rrid="SUSE:Maintenance:9:9")

    with pytest.raises(TemplateNotLoadedError):
        Unload(args, mock_config, MagicMock(), prompt)()

    assert "SUSE:Maintenance:1:1" in reg
