"""Tests for ``mtui.update_workflow.actions.downgrade``."""

from __future__ import annotations

from mtui.update_workflow.actions.downgrade import slmicro, zypper


def test_zypper_downgrade_uses_oldpackage() -> None:
    """The zypper downgrade command allows installing an older version."""
    sub = zypper()["command"].safe_substitute(package="bash", version="1.2-3")
    assert "zypper -n in" in sub
    assert "--oldpackage" in sub
    assert "--force-resolution" in sub
    assert "bash=1.2-3" in sub


def test_slmicro_downgrade_uses_oldpackage() -> None:
    """The transactional-update downgrade command also allows older versions."""
    sub = slmicro()["command"].safe_substitute(package="bash", version="1.2-3")
    assert "transactional-update -c pkg in" in sub
    assert "--oldpackage" in sub
    assert "--force-resolution" in sub
    assert "bash=1.2-3" in sub
