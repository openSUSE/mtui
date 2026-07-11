"""Tests for ``mtui.checks.install``."""

from __future__ import annotations

import logging

import pytest

from mtui.support.exceptions import UpdateError
from mtui.update_workflow.checks.install import install_checks, zypper


@pytest.fixture(autouse=True)
def _silence_logging_format_errors(monkeypatch):
    """Disable logging exception re-raise so all branches are exercisable."""
    monkeypatch.setattr(logging, "raiseExceptions", False)


@pytest.mark.parametrize("exitcode", [0, 100, 101, 102, 103, 106])
def test_zypper_success_exitcodes_return_none(exitcode: int) -> None:
    """Success exitcodes do not raise."""
    assert zypper("h", "", "in pkg", "", exitcode) is None


def test_zypper_exitcode_104_raises_package_not_found() -> None:
    """Exitcode 104 raises ``UpdateError("package not found", host)``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "", 104)


def test_zypper_failure_log_labels_stdout_as_stdout(caplog) -> None:
    """The failure log labels the stdout payload "stdout:", not "stdin:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.install"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "in pkg", "ERR-PAYLOAD", 104)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_zypp_transaction_lock_raises(caplog) -> None:
    """A ZYpp transaction-in-progress stderr raises and labels payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.install"),
        pytest.raises(UpdateError),
    ):
        zypper(
            "h",
            "OUT-PAYLOAD",
            "in pkg",
            "A ZYpp transaction is already in progress.",
            1,
        )
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_system_management_lock_raises(caplog) -> None:
    """A "System management is locked" stderr raises and labels payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.install"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "in pkg", "System management is locked", 1)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_rpm_error_raises(caplog) -> None:
    """``Error:`` in stderr raises ``UpdateError("RPM Error", host)`` labeling "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.install"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "in pkg", "Error: something bad", 1)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_unresolved_dep_raises() -> None:
    """``(c): c`` in stdout raises ``UpdateError("Dependency Error", host)``."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "in pkg", "", 1)


def test_zypper_unknown_error_raises_with_unknown_label(caplog) -> None:
    """An exitcode with no special markers raises "Unknown Error", labeling "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.install"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "in pkg", "", 99)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_install_checks_dispatch() -> None:
    """The ``install_checks`` registry maps SLE keys to ``zypper``."""
    assert install_checks[("15", False)] is zypper
