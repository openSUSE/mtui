"""Extra unit coverage for the refhost-pool helper methods on
:class:`~mtui.test_reports.testreport.TestReport` whose *real* bodies the
selection tests stub out (``_pool_lock_comment``), or whose edge branches
(``release_pool_claims`` skip/error paths, ``_int_cfg`` fallback,
``_disconnect_candidate`` teardown error) the happy-path tests don't reach.

All exercised as unbound methods against tiny stand-ins — no SSH / real
TestReport construction.
"""

from __future__ import annotations

from types import SimpleNamespace
from typing import cast

from mtui.mcp.host_arbiter import HostArbiter
from mtui.test_reports.testreport import TestReport


class _FakeTarget:
    def __init__(self, *, unlock_raises: bool = False, close_raises: bool = False):
        self._unlock_raises = unlock_raises
        self._close_raises = close_raises
        self.unlocked = False
        self.closed = False

    def unlock(self) -> None:
        if self._unlock_raises:
            raise RuntimeError("boom")
        self.unlocked = True

    def close(self) -> None:
        if self._close_raises:
            raise RuntimeError("boom")
        self.closed = True


# --------------------------------------------------------------------------- #
# _pool_lock_comment (real body)                                              #
# --------------------------------------------------------------------------- #
def test_pool_lock_comment_with_owner() -> None:
    rep = SimpleNamespace(id="SUSE:Maintenance:1:1", _host_owner="ws-me")
    assert (
        TestReport._pool_lock_comment(cast("TestReport", rep))
        == "mtui-mcp pool SUSE:Maintenance:1:1 [ws-me]"
    )


def test_pool_lock_comment_without_owner() -> None:
    rep = SimpleNamespace(id="SUSE:Maintenance:1:1", _host_owner=None)
    assert (
        TestReport._pool_lock_comment(cast("TestReport", rep))
        == "mtui-mcp pool SUSE:Maintenance:1:1"
    )


# --------------------------------------------------------------------------- #
# _int_cfg fallback                                                           #
# --------------------------------------------------------------------------- #
def test_int_cfg_returns_default_for_nonint_and_bool() -> None:
    rep_str = SimpleNamespace(config=SimpleNamespace(lock_wait="not-a-number"))
    assert TestReport._int_cfg(cast("TestReport", rep_str), "lock_wait", 7) == 7
    rep_bool = SimpleNamespace(config=SimpleNamespace(lock_wait=True))
    assert TestReport._int_cfg(cast("TestReport", rep_bool), "lock_wait", 7) == 7
    rep_ok = SimpleNamespace(config=SimpleNamespace(lock_wait=1800))
    assert TestReport._int_cfg(cast("TestReport", rep_ok), "lock_wait", 7) == 1800


# --------------------------------------------------------------------------- #
# release_pool_claims edge branches                                           #
# --------------------------------------------------------------------------- #
def test_release_pool_claims_noop_without_arbiter() -> None:
    rep = SimpleNamespace(
        targets={"h": _FakeTarget()}, _host_arbiter=None, _host_owner=None
    )
    # must simply return; no exception, target untouched
    TestReport.release_pool_claims(cast("TestReport", rep))
    assert rep.targets["h"].unlocked is False


def test_release_pool_claims_skips_unowned_and_tolerates_unlock_error() -> None:
    arb = HostArbiter()
    arb.claim("h_other", "someone-else")
    arb.claim("h_mine", "ws-me")
    t_other = _FakeTarget()
    t_mine = _FakeTarget(unlock_raises=True)  # unlock blows up -> swallowed
    rep = SimpleNamespace(
        targets={"h_other": t_other, "h_mine": t_mine},
        _host_arbiter=arb,
        _host_owner="ws-me",
    )

    TestReport.release_pool_claims(cast("TestReport", rep))

    assert t_other.unlocked is False  # not ours -> skipped
    assert arb.owner_of("h_other") == "someone-else"  # left intact
    assert arb.owner_of("h_mine") is None  # ours -> released despite unlock error


# --------------------------------------------------------------------------- #
# _disconnect_candidate teardown error                                        #
# --------------------------------------------------------------------------- #
def test_disconnect_candidate_tolerates_close_error() -> None:
    t = _FakeTarget(close_raises=True)
    rep = SimpleNamespace(
        targets={"h": t}, systems={"h": "x"}, _host_arbiter=None, _host_owner=None
    )
    # close() raises -> must be swallowed (best-effort teardown)
    TestReport._disconnect_candidate(cast("TestReport", rep), "h")
