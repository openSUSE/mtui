"""Tests for ``mtui.checks.downgrade``."""

from __future__ import annotations

import logging

import pytest

from mtui.support.exceptions import UpdateError
from mtui.update_workflow.checks.downgrade import (
    downgrade_checks,
    transactional_update,
    zypper,
)


@pytest.fixture(autouse=True)
def _silence_logging_format_errors(monkeypatch):
    """Some production log calls have mismatched ``%`` args.

    Real production never re-raises these (default ``logging.raiseExceptions``
    is True only when ``sys.flags.dev_mode`` is False, but pytest's
    ``caplog`` propagates exceptions through ``handleError``). Toggle the
    global flag for the duration of these tests so the branches under test
    can be exercised; the production behaviour is unchanged.
    """
    monkeypatch.setattr(logging, "raiseExceptions", False)


def test_zypper_clean_run_returns_none() -> None:
    """A clean run (exit 0, no error markers) returns ``None``."""
    assert zypper("h", "", "in pkg", "", 0) is None


def test_zypper_zypp_lock_raises() -> None:
    """A ZYpp transaction lock in stderr raises ``UpdateError(reason, host)``."""
    with pytest.raises(UpdateError) as ei:
        zypper("h", "", "in pkg", "A ZYpp transaction is already in progress.", 0)
    # Args must be (reason, host) -- previously swapped.
    assert ei.value.reason == "update stack locked"
    assert ei.value.host == "h"


def test_zypper_lock_branches_logging_is_well_formed(monkeypatch) -> None:
    """Both lock branches log with matching %-args (no zero-arg / extra-arg bug).

    With ``logging.raiseExceptions`` re-enabled a wrong arg count would raise a
    ``TypeError`` in ``handleError`` instead of the intended ``UpdateError``.
    """
    monkeypatch.setattr(logging, "raiseExceptions", True)
    with pytest.raises(UpdateError):
        zypper("h", "out", "in pkg", "A ZYpp transaction is already in progress.", 0)
    with pytest.raises(UpdateError):
        zypper("h", "out", "in pkg", "System management is locked", 0)


def test_zypper_system_management_locked_raises(caplog) -> None:
    """A "System management is locked" stderr raises and labels payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.downgrade"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "in pkg", "System management is locked", 0)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_dep_conflict_raises() -> None:
    """A dependency conflict marker ``(c): c`` in stdout raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "in pkg", "", 0)


def test_zypper_exitcode_minus_one_raises() -> None:
    """Exit -1 (``Target.run``'s marker for a timed-out or unrunnable command)
    raises instead of letting the flow continue past an interrupted rollback."""
    with pytest.raises(UpdateError) as ei:
        zypper("h", "", "in pkg", "", -1)
    assert ei.value.reason == "downgrade command timed out or failed to run"
    assert ei.value.host == "h"


def test_zypper_exitcode_104_raises() -> None:
    """Exitcode 104 raises ``UpdateError("Unspecified Error", host)``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "", 104)


def test_zypper_exitcode_106_warns_but_no_raise(caplog) -> None:
    """Exitcode 106 only warns and does not raise."""
    with caplog.at_level(logging.WARNING, logger="mtui.checks.downgrade"):
        result = zypper("h", "", "in pkg", "", 106)
    assert result is None
    assert any("errorcode 106" in r.message for r in caplog.records)


def test_zypper_failure_log_labels_stdout_as_stdout(caplog) -> None:
    """The failure log labels the stdout payload "stdout:", not "stdin:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.downgrade"),
        pytest.raises(UpdateError),
    ):
        zypper(
            "h",
            "OUT-PAYLOAD",
            "in pkg",
            "A ZYpp transaction is already in progress.",
            0,
        )
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_transactional_update_exitcode_minus_one_raises() -> None:
    """The slmicro check raises on a dead command (exit -1): without it the
    registry falls back to a no-op and a dead transactional-update sails on
    to the reboot with no snapshot staged."""
    with pytest.raises(UpdateError) as ei:
        transactional_update("h", "", "tu pkg in", "", -1)
    assert ei.value.reason == "downgrade command timed out or failed to run"
    assert ei.value.host == "h"


def test_transactional_update_clean_run_returns_none() -> None:
    """A clean transactional-update run passes the check."""
    assert transactional_update("h", "", "tu pkg in", "", 0) is None


def test_downgrade_checks_dispatch_keys() -> None:
    """The ``downgrade_checks`` registry maps SLE keys to ``zypper`` and the
    transactional slmicro key to its own check (NOT the no-op fallback)."""
    assert downgrade_checks[("15", False)] is zypper
    assert downgrade_checks[("12", False)] is zypper
    assert downgrade_checks[("slmicro", True)] is transactional_update
