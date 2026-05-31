"""Focused unit tests for the ``Operation`` template introduced in C3.

These tests exercise :class:`mtui.target.hostgroup.Operation` and its two
concrete subclasses directly with stub targets and a stub group, without
constructing a real :class:`HostsGroup`. The pre-existing
``test_perform_install_*`` / ``test_perform_uninstall_*`` cases in
``test_hostgroup.py`` continue to act as integration coverage through the
public ``HostsGroup`` API.
"""

from unittest.mock import MagicMock, call

import pytest

from mtui.support.messages import MissingInstallerError, MissingUninstallerError
from mtui.target.operation import (
    InstallOperation,
    Operation,
    UninstallOperation,
)


def _stub_target(hostname: str, *, transactional: bool = False):
    """Build a MagicMock-backed target with the attributes Operation reads."""
    t = MagicMock()
    t.hostname = hostname
    t.transactional = transactional
    return t


def _stub_group(targets):
    """Build a MagicMock group exposing the surface Operation consumes."""
    group = MagicMock()
    group.data = {t.hostname: t for t in targets}
    return group


def _stub_doer(command: str = "do-it $packages", reboot: str = "reboot-cmd"):
    """Build a ``{command, reboot}`` mapping shaped like Target.doer(role)."""
    return {
        "command": MagicMock(substitute=MagicMock(return_value=command)),
        "reboot": MagicMock(substitute=MagicMock(return_value=reboot)),
    }


def test_operation_collects_commands_and_reboot_per_transactional():
    """``collect()`` returns one entry per host for commands; transactional only for reboot."""
    t1 = _stub_target("h1", transactional=False)
    t1.doer.return_value = _stub_doer("zypper in pkg-a", "reboot-1")
    t2 = _stub_target("h2", transactional=True)
    t2.doer.return_value = _stub_doer("zypper in pkg-a", "systemctl reboot")
    group = _stub_group([t1, t2])

    commands, reboot = InstallOperation(group, ["pkg-a"]).collect()

    assert commands == {"h1": "zypper in pkg-a", "h2": "zypper in pkg-a"}
    # h1 is non-transactional → omitted from reboot map.
    assert reboot == {"h2": "systemctl reboot"}

    # The "command" template is substituted with the joined package list.
    t1.doer.return_value["command"].substitute.assert_called_with(packages="pkg-a")


def test_operation_returns_early_when_doer_raises_missing_error():
    """If doer() raises the configured missing_error, no lock/run/unlock happens."""
    t1 = _stub_target("h1")
    t1.doer.side_effect = MissingInstallerError("opensuse-15.4")
    group = _stub_group([t1])

    InstallOperation(group, ["pkg-a"]).run()

    group.update_lock.assert_not_called()
    group.run.assert_not_called()
    group.unlock.assert_not_called()
    group._reboot.assert_not_called()


def test_operation_always_unlocks_in_finally_when_run_raises():
    """When ``group.run`` raises, the exception propagates but unlock still runs."""
    t1 = _stub_target("h1")
    t1.doer.return_value = _stub_doer()
    group = _stub_group([t1])
    group.run.side_effect = RuntimeError("connection lost")

    with pytest.raises(RuntimeError, match="connection lost"):
        InstallOperation(group, ["pkg-a"]).run()

    group.update_lock.assert_called_once()
    group.unlock.assert_called_once()


def test_operation_check_called_per_target_with_lastN_args():
    """The check callable is invoked once per target with (hostname, lastout, lastin, lasterr, lastexit)."""
    t1 = _stub_target("h1")
    t1.doer.return_value = _stub_doer()
    t1.lastout.return_value = "OUT-1"
    t1.lastin.return_value = "IN-1"
    t1.lasterr.return_value = "ERR-1"
    t1.lastexit.return_value = 0
    check = MagicMock()
    t1.check.return_value = check

    t2 = _stub_target("h2")
    t2.doer.return_value = _stub_doer()
    t2.lastout.return_value = "OUT-2"
    t2.lastin.return_value = "IN-2"
    t2.lasterr.return_value = "ERR-2"
    t2.lastexit.return_value = 1
    # Each target's check() returns its own check callable in production;
    # share one MagicMock here so we can inspect both invocations in one place.
    t2.check.return_value = check

    group = _stub_group([t1, t2])

    InstallOperation(group, ["pkg-a"]).run()

    assert check.call_args_list == [
        call("h1", "OUT-1", "IN-1", "ERR-1", 0),
        call("h2", "OUT-2", "IN-2", "ERR-2", 1),
    ]
    group._reboot.assert_called_once()


def test_install_and_uninstall_operations_use_correct_role_string():
    """``InstallOperation`` looks up the ``"installer"`` role; uninstall looks up ``"uninstaller"``."""
    t1 = _stub_target("h1")
    t1.doer.return_value = _stub_doer()
    group = _stub_group([t1])

    InstallOperation(group, ["pkg"]).run()
    # Both doer() and check() must have been called with the "installer" role.
    assert call("installer") in t1.doer.call_args_list
    assert call("installer") in t1.check.call_args_list
    assert call("uninstaller") not in t1.doer.call_args_list
    assert call("uninstaller") not in t1.check.call_args_list

    t1.reset_mock()
    t1.doer.return_value = _stub_doer()

    UninstallOperation(group, ["pkg"]).run()
    assert call("uninstaller") in t1.doer.call_args_list
    assert call("uninstaller") in t1.check.call_args_list
    assert call("installer") not in t1.doer.call_args_list
    assert call("installer") not in t1.check.call_args_list

    # And the subclasses advertise their configured missing_error sentinel.
    assert InstallOperation.missing_error is MissingInstallerError
    assert UninstallOperation.missing_error is MissingUninstallerError


def test_operation_base_class_cannot_be_instantiated():
    """The abstract base refuses instantiation; both hooks are abstract methods."""
    t1 = _stub_target("h1")
    group = _stub_group([t1])

    with pytest.raises(TypeError, match="abstract"):
        Operation(group, ["pkg"])  # type: ignore[abstract]

    # Both hooks are explicitly marked abstract.
    assert getattr(Operation.get_doer, "__isabstractmethod__", False)
    assert getattr(Operation.get_check, "__isabstractmethod__", False)
