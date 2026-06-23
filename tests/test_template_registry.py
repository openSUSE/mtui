"""Unit tests for :class:`mtui.template_registry.TemplateRegistry`."""

from unittest.mock import MagicMock

import pytest

from mtui.template_registry import TemplateRegistry


class FakeTarget:
    def __init__(self):
        self.closed = False

    def close(self):
        self.closed = True


class FakeHostsGroup(dict):
    """Minimal stand-in for HostsGroup (a dict[str, Target])."""


def make_report(rrid, *, hosts=None):
    report = MagicMock()
    report.id = rrid
    hg = FakeHostsGroup()
    for name in hosts or []:
        hg[name] = FakeTarget()
    report.targets = hg
    return report


@pytest.fixture
def null_report():
    null = MagicMock()
    null.id = ""
    null.targets = FakeHostsGroup()
    return null


@pytest.fixture
def registry(null_report):
    return TemplateRegistry(MagicMock(), null_factory=lambda: null_report)


def test_empty_registry_is_falsey_and_active_is_null(registry, null_report):
    assert len(registry) == 0
    assert not registry
    assert registry.active is null_report


def test_id_is_stable_and_nonempty(registry):
    first = registry.id
    assert first
    assert registry.id == first


def test_add_first_becomes_active(registry):
    r = make_report("SUSE:Maintenance:1:1")
    registry.add(r)
    assert len(registry) == 1
    assert bool(registry)
    assert registry.active is r
    assert "SUSE:Maintenance:1:1" in registry


def test_add_second_does_not_change_active(registry):
    r1 = make_report("SUSE:Maintenance:1:1")
    r2 = make_report("SUSE:Maintenance:2:2")
    registry.add(r1)
    registry.add(r2)
    assert len(registry) == 2
    assert registry.active is r1


def test_get_returns_report(registry):
    r = make_report("SUSE:Maintenance:1:1")
    registry.add(r)
    assert registry.get("SUSE:Maintenance:1:1") is r


def test_get_unknown_raises(registry):
    with pytest.raises(KeyError):
        registry.get("nope")


def test_set_active_flips_pointer(registry):
    r1 = make_report("SUSE:Maintenance:1:1")
    r2 = make_report("SUSE:Maintenance:2:2")
    registry.add(r1)
    registry.add(r2)
    registry.set_active("SUSE:Maintenance:2:2")
    assert registry.active is r2


def test_set_active_unknown_raises(registry):
    with pytest.raises(KeyError):
        registry.set_active("nope")


def test_all_preserves_insertion_order(registry):
    r1 = make_report("a")
    r2 = make_report("b")
    r3 = make_report("c")
    registry.add(r1)
    registry.add(r2)
    registry.add(r3)
    assert registry.all() == [r1, r2, r3]


def test_remove_closes_targets_and_drops(registry):
    r = make_report("SUSE:Maintenance:1:1", hosts=["h1", "h2"])
    targets = r.targets
    h1, h2 = targets["h1"], targets["h2"]
    registry.add(r)
    registry.remove("SUSE:Maintenance:1:1")
    assert h1.closed
    assert h2.closed
    assert len(targets) == 0
    assert len(registry) == 0


def test_remove_active_picks_next(registry, null_report):
    r1 = make_report("a")
    r2 = make_report("b")
    registry.add(r1)
    registry.add(r2)
    registry.remove("a")
    assert registry.active is r2


def test_remove_last_falls_back_to_null(registry, null_report):
    r = make_report("a")
    registry.add(r)
    registry.remove("a")
    assert registry.active is null_report
    assert not registry


def test_remove_nonactive_keeps_active(registry):
    r1 = make_report("a")
    r2 = make_report("b")
    registry.add(r1)
    registry.add(r2)
    registry.remove("b")
    assert registry.active is r1


def test_remove_swallows_close_errors(registry):
    r = make_report("a", hosts=["h1"])
    r.targets["h1"].close = MagicMock(side_effect=RuntimeError("boom"))
    registry.add(r)
    registry.remove("a")  # must not raise
    assert len(registry) == 0
