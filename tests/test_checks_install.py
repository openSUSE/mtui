"""Tests for ``mtui.checks.install``."""

from __future__ import annotations

import logging

import pytest

from mtui.checks.install import install_checks, zypper
from mtui.exceptions import UpdateError


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


def test_zypper_zypp_transaction_lock_raises() -> None:
    """A ZYpp transaction-in-progress stderr raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "A ZYpp transaction is already in progress.", 1)


def test_zypper_system_management_lock_raises() -> None:
    """A "System management is locked" stderr raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "System management is locked", 1)


def test_zypper_rpm_error_raises() -> None:
    """``Error:`` in stderr raises ``UpdateError("RPM Error", host)``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "Error: something bad", 1)


def test_zypper_unresolved_dep_raises() -> None:
    """``(c): c`` in stdout raises ``UpdateError("Dependency Error", host)``."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "in pkg", "", 1)


def test_zypper_unknown_error_raises_with_unknown_label() -> None:
    """An exitcode with no special markers raises ``Unknown Error``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "", 99)


def test_install_checks_dispatch() -> None:
    """The ``install_checks`` registry maps SLE keys to ``zypper``."""
    assert install_checks[("15", False)] is zypper
