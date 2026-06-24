"""Unit tests for :class:`mtui.hosts.host_arbiter.HostArbiter`."""

import threading
import time

from mtui.hosts.host_arbiter import HostArbiter, get_arbiter

OWNER_A = ("reg1", "SUSE:Maintenance:1:1")
OWNER_B = ("reg2", "SUSE:Maintenance:1:1")  # same RRID, different registry


def test_try_acquire_free_then_foreign() -> None:
    arb = HostArbiter()
    assert arb.try_acquire("host1", OWNER_A) is True
    # same owner re-acquires idempotently
    assert arb.try_acquire("host1", OWNER_A) is True
    # different owner is refused
    assert arb.try_acquire("host1", OWNER_B) is False
    assert arb.owner_of("host1") == OWNER_A


def test_acquire_any_picks_first_free() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_B)
    got = arb.acquire_any(["h1", "h2", "h3"], OWNER_A)
    assert got == "h2"
    assert arb.owner_of("h2") == OWNER_A


def test_acquire_any_all_busy_failfast() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_B)
    assert arb.acquire_any(["h1"], OWNER_A, wait=0) is None


def test_acquire_any_empty_candidates() -> None:
    arb = HostArbiter()
    assert arb.acquire_any([], OWNER_A, wait=5) is None


def test_release_wakes_waiter() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_B)
    result: list[str | None] = []

    def waiter() -> None:
        result.append(arb.acquire_any(["h1"], OWNER_A, wait=5, poll=1))

    t = threading.Thread(target=waiter)
    t.start()
    time.sleep(0.2)  # let the waiter block
    arb.release("h1", OWNER_B)
    t.join(timeout=3)
    assert result == ["h1"]
    assert arb.owner_of("h1") == OWNER_A


def test_acquire_any_times_out() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_B)
    start = time.monotonic()
    got = arb.acquire_any(["h1"], OWNER_A, wait=1, poll=1)
    assert got is None
    assert time.monotonic() - start >= 1


def test_release_owner_frees_all() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_A)
    arb.try_acquire("h2", OWNER_A)
    arb.try_acquire("h3", OWNER_B)
    arb.release_owner(OWNER_A)
    assert arb.owner_of("h1") is None
    assert arb.owner_of("h2") is None
    assert arb.owner_of("h3") == OWNER_B


def test_release_only_when_owner_matches() -> None:
    arb = HostArbiter()
    arb.try_acquire("h1", OWNER_A)
    arb.release("h1", OWNER_B)  # not the holder; no-op
    assert arb.owner_of("h1") == OWNER_A


def test_get_arbiter_singleton() -> None:
    assert get_arbiter() is get_arbiter()
