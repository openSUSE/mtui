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


def test_zypper_zypper_in_stdin_exitcode_104_raises() -> None:
    """``zypper`` in stdin + exitcode 104 raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "zypper in pkg", "", 104)


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


def test_zypper_zypp_transaction_lock_raises() -> None:
    """ZYpp transaction lock raises."""
    with pytest.raises(UpdateError):
        zypper("h", "", "echo", "A ZYpp transaction is already in progress.", 0)


def test_zypper_system_management_locked_raises() -> None:
    """System management lock raises."""
    with pytest.raises(UpdateError):
        zypper("h", "", "echo", "System management is locked", 0)


def test_zypper_unresolved_dep_raises() -> None:
    """``(c): c`` in stdout raises."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "echo", "", 0)


def test_zypper_rpm_error_raises() -> None:
    """``Error:`` in stderr raises."""
    with pytest.raises(UpdateError):
        zypper("h", "", "echo", "Error: bad", 0)


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
