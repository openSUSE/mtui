"""Tests for the `downgrade` command."""

from __future__ import annotations

import logging
from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.downgrade import Downgrade
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.exceptions import UpdateError
from mtui.support.messages import NoRefhostsDefinedError, TestReportNotLoadedError
from mtui.types import Package
from mtui.types.rpmver import RPMVersion


def _target(hostname="h1", state="enabled"):
    t = MagicMock()
    t.hostname = hostname
    t.state = state
    t.packages = {}
    return t


def _pkg(name: str, required: str, current: str | None, after: str | None = None):
    p = Package(name)
    p.required = required
    p.current = current
    p.after = after
    return p


def _prompt(targets) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.display = MagicMock()
    p.targets = targets
    p.interactive = True
    return p


def test_downgrade_happy_calls_perform_downgrade(mock_config):
    t1 = _target("h1")
    hg = HostsGroup([t1])
    prompt = _prompt(hg)
    args = Namespace(hosts=None)

    Downgrade(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_downgrade.assert_called_once()


def test_downgrade_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(hosts=None)

    with pytest.raises(NoRefhostsDefinedError):
        Downgrade(args, mock_config, MagicMock(), prompt)()


def test_downgrade_without_metadata_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()


# ---------------------------------------------------------------------------
# Post-flow verification: a half-rollback must be a named ERROR, not a bare
# warning a headless caller scrolls past (the ppc64le trap: the probe timed
# out, zero downgrade commands ran, and "downgrade not completed" was the
# only hint that all 28 packages were still at the update version).
# ---------------------------------------------------------------------------


def test_downgrade_reports_packages_still_at_update_version(mock_config, caplog):
    """Packages at (or above) the update's shipped version after the flow are
    named per host, with versions, at ERROR level -- on EVERY host (no
    short-circuit after the first hit), and the bookkeeping still advances for
    the non-flagged packages."""
    t1 = _target("h1")
    pkg_b = _pkg("pkg-b", required="2.0-1", current="1.0-1", after="0.9-1")
    t1.packages = {
        "pkg-a": _pkg("pkg-a", required="1.5-1", current="1.5-1"),
        "pkg-b": pkg_b,
    }
    t2 = _target("h2")
    pkg_c = _pkg("pkg-c", required="3.0-1", current="3.0-1", after="3.0-1")
    t2.packages = {"pkg-c": pkg_c}
    prompt = _prompt(HostsGroup([t1, t2]))

    with caplog.at_level(logging.INFO, logger="mtui.command.downgrade"):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()

    own = {
        r.getMessage(): r.levelno
        for r in caplog.records
        if r.name == "mtui.command.downgrade"
    }
    assert (
        own.get(
            "h1: still at or above the update's shipped version after "
            "downgrade: pkg-a (at 1.5-1, update ships 1.5-1)"
        )
        == logging.ERROR
    )
    assert (
        own.get(
            "h2: still at or above the update's shipped version after "
            "downgrade: pkg-c (at 3.0-1, update ships 3.0-1)"
        )
        == logging.ERROR
    )
    assert any("downgrade not completed" in m for m in own)
    assert "done" not in own
    # Bookkeeping advanced for the healthy package (no short-circuit).
    assert pkg_b.before == RPMVersion("0.9-1")
    assert pkg_b.after == RPMVersion("1.0-1")
    assert pkg_c.after == RPMVersion("3.0-1")


def test_downgrade_below_required_is_done_and_bookkeeps(mock_config, caplog):
    """A completed rollback reports done; before/after bookkeeping advances."""
    t1 = _target("h1")
    pkg = _pkg("pkg-a", required="1.5-1", current="1.0-1", after="1.5-1")
    t1.packages = {"pkg-a": pkg}
    prompt = _prompt(HostsGroup([t1]))

    with caplog.at_level(logging.INFO, logger="mtui.command.downgrade"):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()

    messages = [r.getMessage() for r in caplog.records]
    assert "done" in messages
    assert not any("after downgrade:" in m for m in messages)
    # before <- old after, after <- current
    assert pkg.before == RPMVersion("1.5-1")
    assert pkg.after == RPMVersion("1.0-1")


def test_downgrade_updateerror_reports_half_rollback(mock_config, caplog):
    """An UpdateError from the workflow (dead probe / dead downgrade command)
    surfaces as an explicit half-rollback ERROR with a verification hint."""
    t1 = _target("h1")
    prompt = _prompt(HostsGroup([t1]))
    prompt.metadata.perform_downgrade.side_effect = UpdateError(
        "package version probe failed", "h1"
    )

    with caplog.at_level(logging.ERROR, logger="mtui.command.downgrade"):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()

    messages = [r.getMessage() for r in caplog.records]
    assert "downgrade failed: h1: package version probe failed" in messages
    assert any("verify with 'rpm -q'" in m for m in messages)
    # The historical sentinel stays greppable on the abort path too.
    assert "downgrade not completed" in messages
    # The flow aborted: no version query, no claim of done.
    t1.query_versions.assert_not_called()


def test_downgrade_updateerror_headless_reraises(mock_config, caplog):
    """Headless (mtui-mcp) callers read structured success, not logs: an
    aborted rollback must fail the command, not end as an ERROR-logged
    return that the tool reports as success."""
    t1 = _target("h1")
    prompt = _prompt(HostsGroup([t1]))
    prompt.interactive = False
    prompt.metadata.perform_downgrade.side_effect = UpdateError(
        "package version probe failed", "h1"
    )

    with (
        caplog.at_level(logging.ERROR, logger="mtui.command.downgrade"),
        pytest.raises(UpdateError),
    ):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()

    assert any("downgrade failed" in r.getMessage() for r in caplog.records)


def test_downgrade_not_completed_headless_raises(mock_config):
    """Packages left at the update version fail the command when headless."""
    t1 = _target("h1")
    t1.packages = {"pkg-a": _pkg("pkg-a", required="1.5-1", current="1.5-1")}
    prompt = _prompt(HostsGroup([t1]))
    prompt.interactive = False

    with pytest.raises(UpdateError, match="downgrade not completed"):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()
