"""Tests for refhost-pool candidate selection.

Two layers:

* ``Refhosts.search_pool`` — returns ``(host, slot)`` pairs (slot = the
  matched test-target query) and, with ``all_locations=True``, aggregates
  candidates across every location (location ignored), de-duplicated by name.
* ``TestReport._claim_first_free`` / ``_claim_pool_candidates`` — the
  connect-time selection that picks one free host per arch, skipping hosts
  locked by another agent and claiming the chosen one.

The selection logic is exercised against a lightweight fake report (the
methods only touch ``add_target`` / ``targets`` / ``systems`` /
``_disconnect_candidate``), so no real SSH or TestReport construction is
needed.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import cast

from mtui.hosts import refhost
from mtui.mcp.host_arbiter import HostArbiter
from mtui.test_reports.testreport import TestReport

REFHOSTS_FIXTURE = Path(__file__).parent / "fixtures" / "refhosts.yml"


# --------------------------------------------------------------------------- #
# Refhosts.search_pool                                                        #
# --------------------------------------------------------------------------- #


def _sles155_x86() -> list:
    return refhost.Attributes.from_testplatform(
        "base=sles(major=15,minor=5);arch=[x86_64]"
    )


def test_search_pool_all_locations_aggregates_across_locations() -> None:
    """all_locations=True draws candidates from every location, with arch."""
    rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
    pool = rh.search_pool(_sles155_x86(), all_locations=True)
    assert {h.name for h, _slot in pool} == {"host-default-x86", "host-nbg-x86"}
    assert all(h.arch == "x86_64" for h, _slot in pool)


def test_search_pool_same_target_shares_one_slot() -> None:
    """Both hosts satisfying one query are poolable: identical slot label."""
    rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
    pool = rh.search_pool(_sles155_x86(), all_locations=True)
    slots = {slot for _h, slot in pool}
    assert len(slots) == 1  # one test target -> one slot -> poolable to one host


def test_search_pool_distinct_service_packs_are_distinct_slots() -> None:
    """Same arch, different product versions -> different slots (both kept)."""
    rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
    attrs = refhost.Attributes.from_testplatform(
        "base=sles(major=15,minor=5);arch=[x86_64]"
    ) + refhost.Attributes.from_testplatform(
        "base=sles(major=12,minor=sp4);arch=[x86_64]"
    )
    pool = rh.search_pool(attrs, all_locations=True)
    slot_by_name = {h.name: slot for h, slot in pool}
    # the 15.5 hosts and the 12-sp4 host must NOT collapse into one slot
    assert slot_by_name["host-default-noaddon"] != slot_by_name["host-nbg-x86"]


def test_search_pool_location_scoped_falls_back_to_default() -> None:
    """Location-scoped search_pool falls back to default, like search."""
    rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
    attrs = refhost.Attributes.from_testplatform(
        "base=sles(major=12,minor=sp4);arch=[x86_64]"
    )
    assert [h.name for h, _slot in rh.search_pool(attrs)] == ["host-default-noaddon"]


def test_search_pool_dedupes_by_name() -> None:
    """A name is returned once even if it matches under several attributes."""
    rh = refhost.Refhosts(REFHOSTS_FIXTURE, location="nuremberg")
    pool = rh.search_pool(_sles155_x86() + _sles155_x86(), all_locations=True)
    names = sorted(h.name for h, _slot in pool)
    assert names == ["host-default-x86", "host-nbg-x86"]


# --------------------------------------------------------------------------- #
# Selection logic (fakes)                                                     #
# --------------------------------------------------------------------------- #


class _FakeLock:
    def __init__(self, *, mine: bool = False, reap: bool = False) -> None:
        self._mine = mine
        self._reap = reap

    def is_mine(self) -> bool:
        return self._mine

    def reap_if_stale(self) -> bool:
        return self._reap

    def locked_by(self) -> str:
        return "otheruser"


class _FakeTarget:
    def __init__(
        self,
        *,
        locked: bool,
        mine: bool = False,
        reap: bool = False,
        lock_raises: bool = False,
    ) -> None:
        self._locked = locked
        self._lock = _FakeLock(mine=mine, reap=reap)
        self._lock_raises = lock_raises
        self.closed = False
        self.unlocked = False
        self.lock_calls: list[str] = []

    def unlock(self) -> None:
        self.unlocked = True
        self._locked = False

    def is_locked(self) -> bool:
        return self._locked

    def locked_by(self) -> str:
        return self._lock.locked_by()

    def try_claim(self, comment: str = "") -> bool:
        # Mirrors Target.try_claim against the fake's flags.
        if self._locked and not self._lock.is_mine() and not self._lock.reap_if_stale():
            return False
        if self._lock_raises:
            return False
        self.lock_calls.append(comment)
        return True

    def close(self) -> None:
        self.closed = True


def _fake_report(targets_map: dict[str, _FakeTarget]) -> TestReport:
    # A duck-typed stand-in: the methods under test only touch add_target /
    # targets / systems / _disconnect_candidate. Cast so the unbound
    # TestReport methods accept it (ty) without building a real TestReport.
    # No arbiter bound -> _claim_first_free dispatches to the remote-only path
    # (these tests exercise that path; the arbiter path is covered separately).
    self = SimpleNamespace(targets={}, systems={}, _host_arbiter=None, _host_owner=None)

    def add_target(name: str) -> None:
        if name in targets_map:
            self.targets[name] = targets_map[name]

    self.add_target = add_target
    self._disconnect_candidate = lambda n: TestReport._disconnect_candidate(
        cast("TestReport", self), n
    )
    self._pool_lock_comment = lambda: "mtui-mcp pool test"
    return cast("TestReport", self)


def test_claim_first_free_picks_first_unlocked_and_claims_it() -> None:
    h1 = _FakeTarget(locked=True)  # busy (locked by other)
    h2 = _FakeTarget(locked=False)  # free -> chosen
    h3 = _FakeTarget(locked=False)
    rep = _fake_report({"h1": h1, "h2": h2, "h3": h3})

    chosen = TestReport._claim_first_free_remote(rep, "x86_64", ["h1", "h2", "h3"])

    assert chosen == "h2"
    assert h2.lock_calls  # claimed (locked)
    assert h1.closed  # busy fallback disconnected once h2 was claimed
    assert "h3" not in rep.targets  # never reached
    assert set(rep.targets) == {"h2"}


def test_claim_first_free_all_busy_returns_first_connected() -> None:
    h1 = _FakeTarget(locked=True)
    h2 = _FakeTarget(locked=True)
    rep = _fake_report({"h1": h1, "h2": h2})

    chosen = TestReport._claim_first_free_remote(rep, "x86_64", ["h1", "h2"])

    assert chosen == "h1"  # kept as the fallback host
    assert set(rep.targets) == {"h1"}
    assert not h1.lock_calls  # not claimed; lock-wait policy applies later


def test_claim_first_free_handles_lock_race() -> None:
    # h1 looks free but its lock() loses the race; h2 then claims.
    h1 = _FakeTarget(locked=False, lock_raises=True)
    h2 = _FakeTarget(locked=False)
    rep = _fake_report({"h1": h1, "h2": h2})

    chosen = TestReport._claim_first_free_remote(rep, "x86_64", ["h1", "h2"])

    assert chosen == "h2"
    assert h2.lock_calls
    assert "h1" not in rep.targets


def test_claim_first_free_reaped_stale_lock_is_claimable() -> None:
    # locked, not mine, but reap_if_stale() removes it -> claimable.
    h1 = _FakeTarget(locked=True, reap=True)
    rep = _fake_report({"h1": h1})

    chosen = TestReport._claim_first_free_remote(rep, "x86_64", ["h1"])

    assert chosen == "h1"
    assert h1.lock_calls


def test_claim_pool_candidates_reduces_only_within_a_slot() -> None:
    """Only same-slot candidates collapse; distinct slots each keep a host.

    Crucially ``a1``/``a2`` (same product+version+arch) pool to one, while
    ``c1`` — same arch but a different service pack — is a different slot and
    is NOT collapsed with them. This is the SLE15-SP5 vs SP7 case.
    """
    rep = SimpleNamespace(
        targets={},
        systems={},
        hostnames={"a1", "a2", "b1", "c1"},
        _candidate_slots={
            "a1": "sles15.5-x86_64-[]",  # same slot as a2 -> reduce
            "a2": "sles15.5-x86_64-[]",
            "b1": "sles15.5-aarch64-[]",  # different arch -> own slot
            "c1": "sles12.4-x86_64-[]",  # same arch, different SP -> own slot
        },
    )
    rep._claim_first_free = lambda slot, names: names[0]

    TestReport._claim_pool_candidates(cast("TestReport", rep))

    assert "a2" not in rep.hostnames  # collapsed into a1
    # one host per distinct slot survives; c1 (different SP) is NOT dropped
    assert {"a1", "b1", "c1"} <= rep.hostnames


# --------------------------------------------------------------------------- #
# Selection logic with the in-process arbiter (cross-workspace)               #
# --------------------------------------------------------------------------- #


def _fake_report_arb(
    targets_map: dict[str, _FakeTarget],
    arbiter: HostArbiter,
    owner: str,
    lock_wait: int = 0,
) -> TestReport:
    self = SimpleNamespace(
        targets={},
        systems={},
        _host_arbiter=arbiter,
        _host_owner=owner,
        config=SimpleNamespace(lock_wait=lock_wait, lock_wait_poll=15),
    )

    def add_target(name: str) -> None:
        if name in targets_map:
            self.targets[name] = targets_map[name]

    self.add_target = add_target
    self._disconnect_candidate = lambda n: TestReport._disconnect_candidate(
        cast("TestReport", self), n
    )
    self._int_cfg = lambda n, d: TestReport._int_cfg(cast("TestReport", self), n, d)
    self._pool_lock_comment = lambda: "mtui-mcp pool test"
    return cast("TestReport", self)


def test_arbiter_skips_host_owned_by_another_workspace() -> None:
    arb = HostArbiter()
    arb.claim("h1", "ws-other")  # held by another workspace in this process
    targets = {
        "h1": _FakeTarget(locked=False),
        "h2": _FakeTarget(locked=False),
        "h3": _FakeTarget(locked=False),
    }
    rep = _fake_report_arb(targets, arb, "ws-me")

    chosen = TestReport._claim_first_free(rep, "slot", ["h1", "h2", "h3"])

    assert chosen == "h2"  # h1 skipped (owned by ws-other) -> h2 claimed
    assert arb.owner_of("h2") == "ws-me"
    assert arb.owner_of("h1") == "ws-other"  # untouched
    assert "h1" not in rep.targets  # never even connected


def test_arbiter_returns_none_when_pool_exhausted_no_wait() -> None:
    arb = HostArbiter()
    arb.claim("h1", "ws-other")
    arb.claim("h2", "ws-other")
    rep = _fake_report_arb(
        {"h1": _FakeTarget(locked=False), "h2": _FakeTarget(locked=False)},
        arb,
        "ws-me",
        lock_wait=0,
    )

    chosen = TestReport._claim_first_free(rep, "slot", ["h1", "h2"])

    assert chosen is None  # all held by others, no wait -> slot unconnected
    assert rep.targets == {}


def test_arbiter_remote_locked_releases_inprocess_and_tries_next() -> None:
    # h1 is free in-process but remote-locked by another *process*; the in-process
    # reservation must be released and the next candidate taken.
    arb = HostArbiter()
    targets = {
        "h1": _FakeTarget(locked=True),  # remote-locked -> try_claim False
        "h2": _FakeTarget(locked=False),
    }
    rep = _fake_report_arb(targets, arb, "ws-me")

    chosen = TestReport._claim_first_free(rep, "slot", ["h1", "h2"])

    assert chosen == "h2"
    assert arb.owner_of("h1") is None  # in-process reservation released
    assert arb.owner_of("h2") == "ws-me"


def test_release_pool_claims_unlocks_remote_and_releases_inprocess() -> None:
    """release_pool_claims removes the remote lock (visible to others) + ownership."""
    arb = HostArbiter()
    arb.claim("h1", "ws-me")
    t1 = _FakeTarget(locked=True)  # we hold its remote lock
    rep = SimpleNamespace(
        targets={"h1": t1},
        _host_arbiter=arb,
        _host_owner="ws-me",
    )

    TestReport.release_pool_claims(cast("TestReport", rep))

    assert t1.unlocked is True  # remote lock removed -> visible-free to others
    assert arb.owner_of("h1") is None  # in-process ownership dropped
