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


def test_zypper_list_command_probes_all_packages_in_one_call() -> None:
    """The version probe runs ONE zypper invocation for the whole list.

    The old per-package ``for`` loop loaded repo metadata once per package
    and, piped through a block-buffered awk, produced no output until the
    last iteration -- on a slow host a long package list blew the SSH
    no-output timeout and the downgrade rolled back nothing.
    """
    sub = zypper()["list_command"].safe_substitute(packages="pkg-a pkg-b pkg-c")
    assert "for p in" not in sub
    assert sub.count("zypper") == 1
    assert "zypper -n se -s --match-exact -t package pkg-a pkg-b pkg-c" in sub


def test_slmicro_list_command_probes_all_packages_in_one_call() -> None:
    """The slmicro probe is the same single-invocation shape as zypper's."""
    sub = slmicro()["list_command"].safe_substitute(packages="pkg-a pkg-b")
    assert "for p in" not in sub
    assert sub.count("zypper") == 1
    assert "zypper -n se -s --match-exact -t package pkg-a pkg-b" in sub


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
