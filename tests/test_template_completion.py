"""Tests for ``-T``/``--template`` tab completion across fan-out commands.

The shared :func:`mtui.cli.completion.template_completion` helper supplies the
``-T``/``--template`` and ``--all-templates`` flag tokens plus every loaded RRID
as a completion candidate, mirroring the ``switch`` / ``unload`` commands. These
tests lock in the helper's contract and verify a representative spread of
fan-out commands wire it into their ``complete`` method.
"""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from mtui.cli.completion import template_completion
from mtui.commands.checkout import Checkout
from mtui.commands.openqa_jobs import OpenQAJobs
from mtui.commands.reboot import Reboot
from mtui.commands.run import Run
from mtui.commands.smelt import SmeltUpdate
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


def _registry(*rrids):
    reg = TemplateRegistry(MagicMock(), null_factory=_null)
    for rrid in rrids:
        reg.add(_report(rrid))
    return reg


def _state(*rrids, hosts=None):
    state = {"templates": _registry(*rrids)}
    if hosts is not None:
        names = MagicMock()
        names.names.return_value = hosts
        state["hosts"] = names
    return state


# --------------------------------------------------------------------------- #
# template_completion helper                                                   #
# --------------------------------------------------------------------------- #


def test_helper_returns_flags_when_no_templates_loaded():
    out = template_completion(_state())
    assert ("-T", "--template") in out
    assert ("--all-templates",) in out


def test_helper_returns_loaded_rrids_as_groups():
    out = template_completion(_state("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:2"))
    assert ("SUSE:Maintenance:1:1",) in out
    assert ("SUSE:Maintenance:2:2",) in out
    assert ("-T", "--template") in out
    assert ("--all-templates",) in out


def test_helper_tolerates_missing_templates_key():
    out = template_completion({})
    assert out == [("-T", "--template"), ("--all-templates",)]


def test_helper_tolerates_none_templates():
    out = template_completion({"templates": None})
    assert out == [("-T", "--template"), ("--all-templates",)]


# --------------------------------------------------------------------------- #
# Per-command wiring                                                           #
# --------------------------------------------------------------------------- #


def test_run_completes_short_template_flag():
    state = _state("SUSE:Maintenance:1:1", hosts=[])
    out = Run.complete(state, "-", "run -", 0, 0)
    assert "-T" in out


def test_run_completes_long_template_flag():
    state = _state("SUSE:Maintenance:1:1", hosts=[])
    out = Run.complete(state, "--", "run --", 0, 0)
    assert "--template" in out
    assert "--all-templates" in out


def test_run_completes_rrid_value():
    state = _state("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:2", hosts=[])
    out = Run.complete(state, "SUSE:Maintenance:2", "run -T SUSE:Maintenance:2", 0, 0)
    assert out == ["SUSE:Maintenance:2:2"]


def test_reboot_completes_rrid_value():
    state = _state("SUSE:Maintenance:7:7", hosts=[])
    out = Reboot.complete(
        state, "SUSE:Maintenance:7", "reboot -T SUSE:Maintenance:7", 0, 0
    )
    assert out == ["SUSE:Maintenance:7:7"]


@pytest.mark.parametrize("command", [Checkout, SmeltUpdate, OpenQAJobs])
def test_template_only_commands_complete_rrid(command):
    """Commands with no host arg still complete loaded RRIDs."""
    state = _state("SUSE:Maintenance:3:3")
    line = f"{command.command} -T SUSE:Maintenance:3"
    out = command.complete(state, "SUSE:Maintenance:3", line, 0, 0)
    assert out == ["SUSE:Maintenance:3:3"]


@pytest.mark.parametrize("command", [Checkout, SmeltUpdate, OpenQAJobs])
def test_template_only_commands_complete_flag(command):
    state = _state("SUSE:Maintenance:3:3")
    out = command.complete(state, "--t", f"{command.command} --t", 0, 0)
    assert "--template" in out
