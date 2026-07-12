"""Tests for ``mtui.checks.update``."""

from __future__ import annotations

import logging

import pytest

from mtui.support.exceptions import UpdateError
from mtui.update_workflow.checks.update import update_checks, zypper


@pytest.fixture(autouse=True)
def _silence_logging_format_errors(monkeypatch):
    """Disable logging exception re-raise so all branches are exercisable."""
    monkeypatch.setattr(logging, "raiseExceptions", False)


def test_zypper_clean_run_returns_none() -> None:
    """A clean run with non-zypper stdin returns ``None``."""
    assert zypper("h", "", "echo hi", "", 0) is None


def test_zypper_zypper_in_stdin_exitcode_104_raises_package_not_found() -> None:
    """Exitcode 104 (ZYPPER_EXIT_INF_CAP_NOT_FOUND) means "package not found".

    Zypper's ZYpp-lock exit code is 7; 104 is capability/package not found,
    so the raised reason must match the ``install`` check's mapping.
    """
    with pytest.raises(UpdateError) as ei:
        zypper("h", "", "zypper in pkg", "", 104)
    assert ei.value.reason == "package not found"
    assert ei.value.host == "h"


def test_zypper_failure_log_labels_stdout_as_stdout(caplog) -> None:
    """The failure log labels the stdout payload "stdout:", not "stdin:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.update"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "zypper in pkg", "ERR-PAYLOAD", 104)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_zypper_in_stdin_exitcode_106_warns(caplog) -> None:
    """``zypper`` in stdin + exitcode 106 warns but does not raise."""
    with caplog.at_level(logging.WARNING, logger="mtui.checks.update"):
        result = zypper("h", "", "zypper in pkg", "", 106)
    assert result is None
    assert any("returns exitcode 106" in r.message for r in caplog.records)


def test_zypper_additional_rpm_output_printed(capsys) -> None:
    """``Additional rpm output:`` lines are printed to stdout."""
    stdout = (
        "blah\nAdditional rpm output:\nwarning: package some thing\nRetrieving thing\n"
    )
    zypper("h", stdout, "echo hi", "", 0)
    captured = capsys.readouterr()
    assert "warning" in captured.out


def test_zypper_zypp_transaction_lock_raises(caplog) -> None:
    """ZYpp transaction lock raises and labels the payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.update"),
        pytest.raises(UpdateError),
    ):
        zypper(
            "h", "OUT-PAYLOAD", "echo", "A ZYpp transaction is already in progress.", 0
        )
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_system_management_locked_raises(caplog) -> None:
    """System management lock raises and labels the payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.update"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "echo", "System management is locked", 0)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_unresolved_dep_raises() -> None:
    """``(c): c`` in stdout raises."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "echo", "", 0)


def test_zypper_rpm_error_raises(caplog) -> None:
    """``Error:`` in stderr raises and labels the payload "stdout:"."""
    with (
        caplog.at_level(logging.CRITICAL, logger="mtui.checks.update"),
        pytest.raises(UpdateError),
    ):
        zypper("h", "OUT-PAYLOAD", "echo", "Error: bad", 0)
    assert any("stdout:\nOUT-PAYLOAD" in r.message for r in caplog.records)


def test_zypper_unsupported_package_printed_no_raise(capsys) -> None:
    """An unsupported-package block is printed but does not raise."""
    stdout = (
        "stuff before\n"
        "The following package is not supported by its vendor:\n"
        "some-package-info\n"
        "\n"
        "after block\n"
    )
    result = zypper("h", stdout, "echo", "", 0)
    captured = capsys.readouterr()
    assert "not supported by its vendor" in captured.out
    assert result is None


def test_update_checks_dispatch() -> None:
    """The ``update_checks`` registry maps SLE keys to ``zypper``."""
    assert update_checks[("15", False)] is zypper
