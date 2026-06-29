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
    """The transactional-update downgrade command allows older versions and takes
    the full ``name=version`` spec list as ``$package`` so all packages downgrade
    in a single fresh snapshot (no ``--continue`` / init snapshot, which left
    packages at the test version after reboot)."""
    cmds = slmicro()
    sub = cmds["command"].safe_substitute(package="bash=1.2-3 vim=2.0-1")
    assert "transactional-update -n pkg in" in sub
    assert "-C" not in cmds["command"].template
    assert "init_snapshot" not in cmds
    assert "--oldpackage" in sub
    assert "--force-resolution" in sub
    assert "bash=1.2-3 vim=2.0-1" in sub
