"""Unit coverage for :meth:`mtui.hosts.target.target.Target.try_claim` and
:meth:`Target.locked_by` — the non-raising pool-claim primitive used by
refhost-pool selection to reserve a candidate against other agents.

``try_claim`` is exercised as an unbound method against a tiny stand-in so the
branch matrix (free / locked-by-other / stale / own-lock / lost-race) is hit
without building a real SSH-backed Target.
"""

from __future__ import annotations

from types import SimpleNamespace
from typing import cast

from mtui.hosts.target.locks import TargetLockedError
from mtui.hosts.target.target import Target


def _stub(
    *, locked: bool, mine: bool, stale: bool, raise_on_lock: bool = False
) -> Target:
    obj = SimpleNamespace()
    obj.is_locked = lambda: locked
    obj._lock = SimpleNamespace(
        is_mine=lambda: mine,
        reap_if_stale=lambda: stale,
        locked_by=lambda: "someone",
    )

    def lock(comment: str = "") -> None:
        if raise_on_lock:
            raise TargetLockedError

    obj.lock = lock
    return cast("Target", obj)


def test_try_claim_free_host_is_claimed() -> None:
    assert Target.try_claim(_stub(locked=False, mine=False, stale=False), "c") is True


def test_try_claim_locked_by_other_not_stale_declines() -> None:
    assert Target.try_claim(_stub(locked=True, mine=False, stale=False), "c") is False


def test_try_claim_stale_lock_is_reaped_then_claimed() -> None:
    assert Target.try_claim(_stub(locked=True, mine=False, stale=True), "c") is True


def test_try_claim_own_lock_proceeds() -> None:
    assert Target.try_claim(_stub(locked=True, mine=True, stale=False), "c") is True


def test_try_claim_lost_race_returns_false() -> None:
    # passes the pre-check (free) but lock() loses the race / raises.
    assert (
        Target.try_claim(
            _stub(locked=False, mine=False, stale=False, raise_on_lock=True), "c"
        )
        is False
    )


def test_locked_by_delegates_to_lock() -> None:
    obj = cast(
        "Target", SimpleNamespace(_lock=SimpleNamespace(locked_by=lambda: "bob"))
    )
    assert Target.locked_by(obj) == "bob"
