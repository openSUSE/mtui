"""Tests for ``mtui.checks.prepare``."""

from __future__ import annotations

import logging

import pytest

from mtui.support.exceptions import UpdateError
from mtui.update_workflow.checks.prepare import prepare_checks, zypper


@pytest.fixture(autouse=True)
def _silence_logging_format_errors(monkeypatch):
    """Disable logging exception re-raise so all branches are exercisable."""
    monkeypatch.setattr(logging, "raiseExceptions", False)


def test_zypper_clean_run_returns_none() -> None:
    """A clean run returns ``None``."""
    assert zypper("h", "", "in pkg", "", 0) is None


def test_zypper_zypp_lock_raises() -> None:
    """A ZYpp transaction lock stderr raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "A ZYpp transaction is already in progress.", 0)


def test_zypper_system_management_locked_raises() -> None:
    """A "System management is locked" stderr raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "System management is locked", 0)


def test_zypper_unresolved_dep_raises() -> None:
    """``(c): c`` in stdout raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "(c): c", "in pkg", "", 0)


def test_zypper_rpm_error_raises() -> None:
    """``Error:`` in stderr raises ``UpdateError``."""
    with pytest.raises(UpdateError):
        zypper("h", "", "in pkg", "Error: foo", 0)


def test_prepare_checks_dispatch() -> None:
    """The ``prepare_checks`` registry maps SLE keys to ``zypper``."""
    assert prepare_checks[("15", False)] is zypper
