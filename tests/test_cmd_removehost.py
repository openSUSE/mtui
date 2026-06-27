"""Tests for the `remove_host` command."""

from __future__ import annotations

from argparse import Namespace
from typing import cast
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.removehost import RemoveHost
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import HostIsNotConnectedError


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg, systems) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.systems = systems
    p.display = MagicMock()
    p.targets = hg
    return p


def test_remove_host_happy_closes_and_pops(mock_config):
    t = _target("h1")
    hg = HostsGroup([t])
    systems = {"h1": MagicMock()}
    prompt = _prompt(hg, systems)
    args = Namespace(hosts=None)

    # Run executor.submit callables inline so the test does not depend on threads.
    def fake_submit(fn, *a, **kw):
        fn(*a, **kw)
        return MagicMock()

    with (
        patch("mtui.commands.removehost.ContextExecutor") as tpe,
        patch("mtui.commands.removehost.concurrent.futures.wait"),
    ):
        executor = MagicMock()
        executor.submit.side_effect = fake_submit
        tpe.return_value.__enter__.return_value = executor

        RemoveHost(args, mock_config, MagicMock(), prompt)()

    t.close.assert_called_once_with()
    # the in-process pool-arbiter claim must be released too (close() only drops
    # the remote lock files) -- otherwise the host stays "busy" for the server's
    # lifetime. See TestReport.release_pool_claim.
    prompt.metadata.release_pool_claim.assert_called_once_with("h1")
    assert "h1" not in hg
    assert "h1" not in systems


def test_remove_host_unknown_propagates(mock_config):
    bad_targets = MagicMock()
    bad_targets.select.side_effect = HostIsNotConnectedError("ghost")
    prompt = _prompt(bad_targets, {})
    args = Namespace(hosts=["ghost"])
    with pytest.raises(HostIsNotConnectedError):
        RemoveHost(args, mock_config, MagicMock(), prompt)()


def test_release_pool_claim_frees_host_for_next_owner():
    """remove_host's release_pool_claim drops the in-process arbiter claim so a
    later template (a different owner) can acquire the host.

    Regression for the stale-claim bug: remove_host removed the remote lock
    files but left HostArbiter._owner mapping the host to the old template, so
    the scarce ppc64le/s390x slots stayed "all candidates busy" until the
    mtui-mcp server was restarted.
    """
    from types import SimpleNamespace

    from mtui.hosts.host_arbiter import HostArbiter
    from mtui.test_reports.testreport import TestReport

    arb = HostArbiter()
    owner_a = ("regA", "SUSE:SLFO:1.2:1")
    owner_b = ("regB", "SUSE:SLFO:1.2:2")
    assert arb.acquire_any(["bianca"], owner_a) == "bianca"
    # while owner_a holds it, owner_b cannot get it
    assert arb.acquire_any(["bianca"], owner_b, wait=0) is None

    report = SimpleNamespace(
        _pool_claims={"bianca"},
        _slot_candidates={("SLES", "16-0", "ppc64le", ()): ["bianca"]},
        _arbiter=arb,
        _owner=owner_a,
    )
    TestReport.release_pool_claim(cast(TestReport, report), "bianca")

    assert arb.owner_of("bianca") is None
    assert report._pool_claims == set()
    assert report._slot_candidates == {}
    # the next template/owner can now claim the freed host
    assert arb.acquire_any(["bianca"], owner_b) == "bianca"


def test_release_pool_claim_noop_without_pool_selection():
    """Safe/idempotent when pool selection was never used (arbiter/owner None)."""
    from types import SimpleNamespace

    from mtui.test_reports.testreport import TestReport

    report = SimpleNamespace(
        _pool_claims=set(), _slot_candidates={}, _arbiter=None, _owner=None
    )
    TestReport.release_pool_claim(cast(TestReport, report), "h1")  # must not raise
    assert report._pool_claims == set()
    assert report._slot_candidates == {}


def test_release_pool_claim_keeps_sibling_candidates():
    """Removing one host drops only it from the slot; siblings stay as backups.

    The slot is pruned only once empty, so a backup-refhost fallback for the
    same slot is still possible after one candidate is removed.
    """
    from types import SimpleNamespace

    from mtui.hosts.host_arbiter import HostArbiter
    from mtui.test_reports.testreport import TestReport

    arb = HostArbiter()
    owner = ("regA", "SUSE:SLFO:1.2:1")
    slot = ("SLES", "16-0", "ppc64le", ())
    assert arb.acquire_any(["bianca"], owner) == "bianca"

    report = SimpleNamespace(
        _pool_claims={"bianca"},
        _slot_candidates={slot: ["bianca", "rosa"]},
        _arbiter=arb,
        _owner=owner,
    )
    TestReport.release_pool_claim(cast(TestReport, report), "bianca")

    # only "bianca" is gone; the sibling "rosa" remains a fallback candidate
    assert report._slot_candidates == {slot: ["rosa"]}
    assert report._pool_claims == set()
    assert arb.owner_of("bianca") is None
