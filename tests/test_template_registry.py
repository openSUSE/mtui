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


def test_add_ignores_empty_rrid_sentinel(registry):
    # A NullTestReport (failed load) has an empty RRID; it must never be keyed
    # into the registry, else it becomes a phantom entry that breaks fan-out.
    null = make_report("")
    registry.add(null)
    assert len(registry) == 0
    assert not registry
    assert "" not in registry


def test_add_sentinel_does_not_disturb_loaded_templates(registry):
    r = make_report("SUSE:Maintenance:1:1")
    registry.add(r)
    registry.add(make_report(""))  # failed load mid-session
    assert len(registry) == 1
    assert registry.active is r
    assert registry.rrids() == ["SUSE:Maintenance:1:1"]


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


def test_add_replace_tears_down_old_report(registry):
    # Re-adding an RRID with a DIFFERENT report object (e.g. ``regenerate``
    # rebuilding the template without a prior ``remove``) must tear the old
    # report down instead of silently dropping it: release its pool claims,
    # close its refhost connections, and clear its host group.
    old = make_report("SUSE:Maintenance:1:1", hosts=["h1", "h2"])
    old_targets = old.targets
    h1, h2 = old_targets["h1"], old_targets["h2"]
    registry.add(old)
    new = make_report("SUSE:Maintenance:1:1", hosts=["h3"])
    registry.add(new)
    # Old report's connections closed and host group emptied.
    assert h1.closed
    assert h2.closed
    assert len(old_targets) == 0
    old.release_pool_claims.assert_called_once()
    # Registry now serves the NEW report for that RRID; active still resolves.
    assert len(registry) == 1
    assert registry.get("SUSE:Maintenance:1:1") is new
    assert registry.active is new


def test_add_same_object_is_noop(registry):
    # Re-adding the identical object must NOT tear it down: its connections
    # stay open and its pool claims are not released.
    r = make_report("SUSE:Maintenance:1:1", hosts=["h1"])
    h1 = r.targets["h1"]
    registry.add(r)
    registry.add(r)  # idempotent re-add of the same object
    assert not h1.closed
    assert len(r.targets) == 1
    r.release_pool_claims.assert_not_called()
    assert registry.get("SUSE:Maintenance:1:1") is r
    assert registry.active is r


def test_add_sentinel_does_not_tear_down_loaded_report(registry):
    # A failed load (empty-RRID sentinel) mid-session must leave the currently
    # loaded report fully intact: no pool claims released, no targets closed.
    r = make_report("SUSE:Maintenance:1:1", hosts=["h1"])
    h1 = r.targets["h1"]
    registry.add(r)
    registry.add(make_report(""))  # failed load must not tear down the live one
    assert not h1.closed
    assert len(r.targets) == 1
    r.release_pool_claims.assert_not_called()
    assert registry.active is r
    assert registry.rrids() == ["SUSE:Maintenance:1:1"]
