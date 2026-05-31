"""Tests for ``mtui.update_workflow.actions.prepare``."""

from __future__ import annotations

import pytest

from mtui.support.messages import MissingPreparerError
from mtui.update_workflow.actions.prepare import (
    preparer,
    slm_prepare,
    yum_prepare,
    zypper_prepare,
)


def test_yum_prepare_with_testing_true_includes_repo() -> None:
    """When ``testing=True`` the ``--disablerepo`` flag must be omitted."""
    cmds = yum_prepare(force=False, testing=True)
    sub = cmds["command"].safe_substitute(package="bash")
    assert "--disablerepo" not in sub


def test_yum_prepare_default_disables_testing_repo() -> None:
    """Default ``yum_prepare`` disables ``*testing*`` repositories."""
    cmds = yum_prepare()
    sub = cmds["command"].safe_substitute(package="bash")
    assert "--disablerepo=*testing*" in sub


def test_slm_prepare_force_adds_force_resolution() -> None:
    """``slm_prepare(force=True)`` adds ``--force-resolution`` and basics."""
    cmds = slm_prepare(force=True)
    sub = cmds["command"].safe_substitute(package="bash")
    assert "--force-resolution" in sub
    assert "reboot" in cmds
    assert "start_command" in cmds


def test_slm_prepare_default_no_force() -> None:
    """Default ``slm_prepare`` does not include ``--force-resolution``."""
    cmds = slm_prepare()
    sub = cmds["command"].safe_substitute(package="bash")
    assert "--force-resolution" not in sub


def test_zypper_prepare_force_adds_force_resolution() -> None:
    """``zypper_prepare(force=True)`` adds ``--force-resolution`` everywhere."""
    cmds = zypper_prepare(force=True)
    assert "--force-resolution" in cmds["command"].safe_substitute(package="bash")
    assert "--force-resolution" in cmds["installed_only"].safe_substitute(
        package="bash"
    )


def test_preparer_dispatch_known_key() -> None:
    """The ``preparer`` registry returns ``zypper_prepare`` for known keys."""
    fn = preparer[("15", False)]
    assert fn is zypper_prepare


def test_preparer_missing_key_raises_missing_preparer_error() -> None:
    """Unknown keys raise ``MissingPreparerError``."""
    with pytest.raises(MissingPreparerError):
        _ = preparer[("nonexistent", False)]
