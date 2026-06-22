"""Tests for :class:`mtui.mcp.host_arbiter.HostArbiter`.

The arbiter is the in-process counterpart of the remote refhost lock: it keeps
two named workspaces in one ``mtui-mcp`` process (same pid) from driving the
same pool host, and lets a workspace queue for a busy host.
"""

from __future__ import annotations

import threading
import time

from mtui.mcp.host_arbiter import HostArbiter


def test_claim_and_owner_of() -> None:
    a = HostArbiter()
    assert a.owner_of("h1") is None
    assert a.claim("h1", "ws-a") is True
    assert a.owner_of("h1") == "ws-a"


def test_claim_by_another_is_refused_reclaim_by_owner_ok() -> None:
    a = HostArbiter()
    assert a.claim("h1", "ws-a") is True
    assert a.claim("h1", "ws-b") is False  # held by ws-a
    assert a.claim("h1", "ws-a") is True  # re-claim by owner is fine
    assert a.owner_of("h1") == "ws-a"


def test_release_frees_host() -> None:
    a = HostArbiter()
    a.claim("h1", "ws-a")
    a.release("h1", "ws-b")  # not the owner -> no-op
    assert a.owner_of("h1") == "ws-a"
    a.release("h1", "ws-a")
    assert a.owner_of("h1") is None


def test_release_owner_frees_all_its_hosts() -> None:
    a = HostArbiter()
    a.claim("h1", "ws-a")
    a.claim("h2", "ws-a")
    a.claim("h3", "ws-b")
    a.release_owner("ws-a")
    assert a.owner_of("h1") is None
    assert a.owner_of("h2") is None
    assert a.owner_of("h3") == "ws-b"  # untouched


def test_acquire_any_reserves_a_free_candidate() -> None:
    a = HostArbiter()
    a.claim("h1", "ws-other")
    got = a.acquire_any(["h1", "h2", "h3"], "ws-me", timeout=0)
    assert got == "h2"  # h1 held by another -> first free is h2
    assert a.owner_of("h2") == "ws-me"


def test_acquire_any_none_when_all_held_and_no_wait() -> None:
    a = HostArbiter()
    a.claim("h1", "ws-other")
    a.claim("h2", "ws-other")
    assert a.acquire_any(["h1", "h2"], "ws-me", timeout=0) is None


def test_acquire_any_already_owned_by_me_counts_as_free() -> None:
    a = HostArbiter()
    a.claim("h1", "ws-me")
    assert a.acquire_any(["h1"], "ws-me", timeout=0) == "h1"


def test_acquire_any_queues_until_released() -> None:
    """A waiter blocks until another owner releases, then acquires it."""
    a = HostArbiter()
    a.claim("h1", "ws-other")

    def _release_soon() -> None:
        time.sleep(0.2)
        a.release("h1", "ws-other")

    t = threading.Thread(target=_release_soon)
    t.start()
    start = time.monotonic()
    got = a.acquire_any(["h1"], "ws-me", timeout=5, poll=0.05)
    elapsed = time.monotonic() - start
    t.join()
    assert got == "h1"
    assert a.owner_of("h1") == "ws-me"
    assert elapsed >= 0.15  # actually waited for the release
