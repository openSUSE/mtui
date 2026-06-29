"""Tests for the mtui hostgroup module."""

import logging
from pathlib import Path
from unittest.mock import MagicMock, PropertyMock, patch

import pytest

from mtui.hosts.target.hostgroup import HostsGroup
from mtui.hosts.target.locks import TargetLockedError
from mtui.support.exceptions import UpdateError
from mtui.support.messages import HostIsNotConnectedError
from mtui.types import Workflow


def _stub_target(
    hostname: str,
    *,
    transactional: bool = False,
    is_locked: bool = False,
    is_mine: bool = True,
):
    """A bare MagicMock-backed target with the attributes HostsGroup reads."""
    t = MagicMock()
    t.hostname = hostname
    t.transactional = transactional
    t.is_locked.return_value = is_locked
    lock = MagicMock()
    lock.is_mine.return_value = is_mine
    type(t)._lock = PropertyMock(return_value=lock)
    return t


def _doer_dict(**templates: str):
    """Build a ``{name: Template-like}`` mapping for get_*er return values."""
    out = {}
    for name, value in templates.items():
        tpl = MagicMock()
        tpl.substitute.return_value = value
        tpl.safe_substitute.return_value = value
        out[name] = tpl
    return out


# --- Initialization and selection ---


def test_hostgroup_init():
    """Test HostsGroup initialization creates correct mapping."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "host1.example.com"
    t2.hostname = "host2.example.com"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]

    assert len(hg) == 2
    assert "host1.example.com" in hg
    assert "host2.example.com" in hg
    assert hg["host1.example.com"] is t1


def test_hostgroup_init_empty():
    """Test HostsGroup initialization with empty list."""
    hg = HostsGroup([])
    assert len(hg) == 0
    assert hg.names() == []


def test_hostgroup_select_all():
    """Test select() with no args returns self."""
    t1 = MagicMock()
    t1.hostname = "h1"
    hg = HostsGroup([t1])  # type: ignore[arg-type]

    selected = hg.select()
    assert selected is hg


def test_hostgroup_select_enabled_only():
    """Test select(enabled=True) filters out disabled hosts."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"
    t1.state = "enabled"
    t2.state = "disabled"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    selected = hg.select(enabled=True)

    assert len(selected) == 1
    assert "h1" in selected
    assert "h2" not in selected


def test_hostgroup_select_by_hostname():
    """Test select() with specific hostnames."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    selected = hg.select(["h1"])

    assert len(selected) == 1
    assert "h1" in selected


def test_hostgroup_select_nonexistent_host_raises():
    """Test select() with unknown hostname raises HostIsNotConnectedError."""
    t1 = MagicMock()
    t1.hostname = "h1"
    hg = HostsGroup([t1])  # type: ignore[arg-type]

    with pytest.raises(HostIsNotConnectedError):
        hg.select(["unknown-host"])


def test_hostgroup_select_enabled_and_by_hostname():
    """Test select() filtering by hostname AND enabled state."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"
    t1.state = "disabled"
    t2.state = "enabled"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    selected = hg.select(["h1", "h2"], enabled=True)

    assert len(selected) == 1
    assert "h2" in selected


# --- Lock/unlock delegation ---


def test_hostgroup_unlock_delegates():
    """Test unlock() calls unlock on every target."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    hg.unlock("test_comment")

    t1.unlock.assert_called_once_with("test_comment")
    t2.unlock.assert_called_once_with("test_comment")


def test_hostgroup_unlock_suppresses_target_locked_error():
    """Test unlock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.unlock.side_effect = TargetLockedError("locked by someone")

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.unlock()  # should not raise


def test_hostgroup_lock_delegates():
    """Test lock() calls lock on every target."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    hg.lock("comment")

    t1.lock.assert_called_once_with("comment")
    t2.lock.assert_called_once_with("comment")


def test_hostgroup_lock_suppresses_target_locked_error():
    """Test lock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.lock.side_effect = TargetLockedError("locked")

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.lock()  # should not raise


# --- Query and history delegation ---


def test_hostgroup_query_versions():
    """Test query_versions delegates to each target."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"
    t1.query_package_versions.return_value = {"pkg": "1.0"}
    t2.query_package_versions.return_value = {"pkg": "2.0"}

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    result = hg.query_versions(["pkg"])

    assert len(result) == 2
    t1.query_package_versions.assert_called_once_with(["pkg"])
    t2.query_package_versions.assert_called_once_with(["pkg"])


def test_hostgroup_add_history():
    """Test add_history delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.add_history("data")

    t1.add_history.assert_called_once_with("data")


def test_hostgroup_names():
    """Test names() returns all hostnames."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "alpha"
    t2.hostname = "beta"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]
    names = hg.names()

    assert set(names) == {"alpha", "beta"}


# --- update_lock ---


def test_update_lock_locks_unlocked_hosts():
    """Test update_lock() locks hosts that are not locked."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.update_lock()

    t1.lock.assert_called_once()


def test_update_lock_raises_when_locked_by_other():
    """Test update_lock() raises UpdateError when host is locked by another user."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = True

    # Use configure_mock to set _lock since MagicMock has an internal _lock attribute
    mock_lock = MagicMock()
    mock_lock.is_mine.return_value = False
    mock_lock.time.return_value = "Monday, 01.01.2024 12:00 UTC"
    mock_lock.locked_by.return_value = "otheruser"
    mock_lock.comment.return_value = ""
    type(t1)._lock = PropertyMock(return_value=mock_lock)

    hg = HostsGroup([t1])  # type: ignore[arg-type]

    with pytest.raises(UpdateError, match="Hosts locked"):
        hg.update_lock()


# --- perform_install ---


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_install_runs_and_unlocks(mock_run):
    """Test perform_install runs commands and always unlocks."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False
    t1.transactional = False
    t1.doer.return_value = {
        "command": MagicMock(substitute=MagicMock(return_value="zypper in pkg")),
        "reboot": MagicMock(substitute=MagicMock(return_value="")),
    }
    t1.check.return_value = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.perform_install(["pkg"])

    # Verify unlock was called (cleanup always happens)
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_install_unlocks_on_error(mock_run):
    """Test perform_install unlocks even when commands raise."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False
    t1.transactional = False
    t1.doer.return_value = {
        "command": MagicMock(substitute=MagicMock(return_value="zypper in pkg")),
        "reboot": MagicMock(substitute=MagicMock(return_value="")),
    }

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    # Make run raise
    mock_run.return_value.run.side_effect = RuntimeError("connection lost")

    with pytest.raises(RuntimeError, match="connection lost"):
        hg.perform_install(["pkg"])

    # unlock must still be called
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.HostsGroup._fanout_set_repo")
@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_prepare_dependency_error_logged_without_traceback(
    mock_run, mock_fanout, caplog
):
    """An UpdateError from the preparer check is reported cleanly (no traceback)."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.transactional = False
    t1.is_locked.return_value = False
    t1.lasterr.return_value = ""  # pass the early "Failed to prepare host" guard
    # The preparer check raises the expected dependency error.
    t1.check.return_value = MagicMock(side_effect=UpdateError("Dependency Error", "h1"))

    hg = HostsGroup([t1])  # type: ignore[arg-type]

    with caplog.at_level(logging.ERROR, logger="mtui.hosts.target.hostgroup"):
        hg.perform_prepare(
            ["pkg"], MagicMock(), force=False, installed_only=False, testing=False
        )

    # Clean, actionable error -- not a stack-trace dump.
    failures = [
        r
        for r in caplog.records
        if "Prepare failed: h1: Dependency Error" in r.getMessage()
    ]
    assert len(failures) == 1
    assert failures[0].exc_info is None  # logger.error, not logger.exception
    # The broad "Error during prepare operation" traceback path must NOT fire.
    assert not any(
        "Error during prepare operation" in r.getMessage() for r in caplog.records
    )
    # Cleanup still happens.
    t1.unlock.assert_called()


# --- Report methods ---


def test_report_self_delegates():
    """Test report_self delegates to each target with a sink."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.report_self(sink)

    t1.reporter.self_.assert_called_once_with(sink)


def test_report_locks_delegates():
    """Test report_locks delegates to each target (zypper lock by default)."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.report_locks(sink)

    t1.reporter.locks.assert_called_once_with(sink)
    t1.reporter.pool_locks.assert_not_called()


def test_report_locks_pool_delegates_to_pool_locks():
    """``report_locks(pool=True)`` delegates to the pool reporter."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.report_locks(sink, pool=True)

    t1.reporter.pool_locks.assert_called_once_with(sink)
    t1.reporter.locks.assert_not_called()


def test_pool_unlock_delegates_to_each_target():
    """``pool_unlock`` calls ``pool_unlock`` on every target, suppressing errors."""
    t1 = MagicMock()
    t1.hostname = "h1"

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.pool_unlock(force=True)

    t1.pool_unlock.assert_called_once_with(force=True)


def test_report_timeout_delegates():
    """Test report_timeout delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.report_timeout(sink)

    t1.reporter.timeout.assert_called_once_with(sink)


def test_report_products_delegates():
    """Test report_products delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.report_products(sink)

    t1.reporter.products.assert_called_once_with(sink)


# ---------------------------------------------------------------------------
# Group-level sftp wrappers
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.FileDownload")
def test_sftp_get_delegates_to_filedownload(mock_dl):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_get(Path("/remote/x"), Path("/local/x"))
    mock_dl.assert_called_once()
    mock_dl.return_value.run.assert_called_once()


@patch("mtui.hosts.target.hostgroup.FileUpload")
def test_sftp_put_delegates_to_fileupload(mock_ul):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_put(Path("/local/x"), Path("/remote/x"))
    mock_ul.assert_called_once()
    mock_ul.return_value.run.assert_called_once()


@patch("mtui.hosts.target.hostgroup.FileDelete")
def test_sftp_remove_delegates_to_filedelete(mock_rm):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_remove(Path("/remote/x"))
    mock_rm.assert_called_once()
    mock_rm.return_value.run.assert_called_once()


# ---------------------------------------------------------------------------
# _reboot
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_reboot_runs_commands_and_reconnects(mock_run):
    """Non-empty reboot dict fires the reboot then reconnects per host."""
    t1 = _stub_target("h1", transactional=True)
    hg = HostsGroup([t1])
    hg._reboot({"h1": "systemctl reboot"})
    # Fire-and-forget reboot (not via the normal run/RunCommand path)...
    t1.reboot.assert_called_once_with("systemctl reboot")
    mock_run.return_value.run.assert_not_called()
    # ...then a robust reconnect with retries + backoff.
    t1.reconnect.assert_called_once_with(retry=10, backoff=True)


def test_reboot_empty_dict_is_noop():
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg._reboot({})
    t1.reconnect.assert_not_called()


def test_reboot_all_fires_and_reconnects_each():
    """reboot() fire-and-forgets the command then reconnects every host."""
    t1 = _stub_target("h1")
    t2 = _stub_target("h2")
    # boot id changes (before -> after) so the reboot is confirmed.
    t1.boot_id.side_effect = ["id-h1-before", "id-h1-after"]
    t2.boot_id.side_effect = ["id-h2-before", "id-h2-after"]
    hg = HostsGroup([t1, t2])
    hg.reboot()
    for t in (t1, t2):
        t.reboot.assert_called_once_with("systemctl reboot")
        t.reconnect.assert_called_once_with(retry=10, backoff=True)
        t.lock.assert_not_called()  # no relock comment -> no relock


def test_reboot_all_relocks_when_comment_given():
    """A relock_comment re-applies the lock after reconnect (PI lock survives)."""
    t1 = _stub_target("h1")
    t1.boot_id.side_effect = ["before", "after"]
    hg = HostsGroup([t1])
    hg.reboot(relock_comment="testing of SUSE:PI:34556:1")
    t1.reboot.assert_called_once_with("systemctl reboot")
    t1.reconnect.assert_called_once_with(retry=10, backoff=True)
    t1.lock.assert_called_once_with("testing of SUSE:PI:34556:1")


def test_reboot_all_errors_when_boot_id_unchanged(caplog):
    """An unchanged boot id after reboot is reported as an error."""
    t1 = _stub_target("h1")
    t1.boot_id.side_effect = ["same-boot-id", "same-boot-id"]
    hg = HostsGroup([t1])
    with caplog.at_level(logging.ERROR, logger="mtui.hosts.target.hostgroup"):
        hg.reboot()
    assert any("boot id unchanged" in r.getMessage() for r in caplog.records)


def test_reboot_all_no_error_when_boot_id_changes(caplog):
    """A changed boot id confirms the reboot; no error is logged."""
    t1 = _stub_target("h1")
    t1.boot_id.side_effect = ["before", "after"]
    hg = HostsGroup([t1])
    with caplog.at_level(logging.ERROR, logger="mtui.hosts.target.hostgroup"):
        hg.reboot()
    assert not any("boot id unchanged" in r.getMessage() for r in caplog.records)


def test_reboot_all_empty_group_is_noop():
    hg = HostsGroup([])
    hg.reboot()  # should not raise


def test_reboot_all_logs_clean_host_list_and_back_up(caplog):
    """Reboot logs a sorted comma-joined host list (not a Python list repr)."""
    t1 = _stub_target("h2")
    t2 = _stub_target("h1")
    t1.boot_id.side_effect = ["b2", "a2"]
    t2.boot_id.side_effect = ["b1", "a1"]
    hg = HostsGroup([t1, t2])
    with caplog.at_level(logging.INFO, logger="mtui.target.hostgroup"):
        hg.reboot()
    msgs = [r.getMessage() for r in caplog.records]
    assert "Rebooting: h1, h2" in msgs
    assert "h1 is back up" in msgs
    assert "h2 is back up" in msgs
    # No raw list repr leaked into the output.
    assert not any("['" in m for m in msgs)


# ---------------------------------------------------------------------------
# update_lock — comment-logging branch
# ---------------------------------------------------------------------------


def test_update_lock_logs_other_user_comment(caplog):
    """When the locking user left a comment, it is logged at info level."""
    t1 = _stub_target("h1", is_locked=True, is_mine=False)
    t1._lock.time.return_value = "Mon 01.01.2024"
    t1._lock.locked_by.return_value = "carol"
    t1._lock.comment.return_value = "investigating bug"
    hg = HostsGroup([t1])
    with (
        caplog.at_level("INFO", logger="mtui.target.hostgroup"),
        pytest.raises(UpdateError),
    ):
        hg.update_lock()
    assert any("investigating bug" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# perform_uninstall
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_uninstall_runs_and_unlocks(mock_run):
    t1 = _stub_target("h1")
    t1.doer.return_value = _doer_dict(command="zypper rm pkg", reboot="")
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_uninstall(["pkg"])
    mock_run.assert_called()
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_uninstall_unlocks_on_error(mock_run):
    t1 = _stub_target("h1")
    t1.doer.return_value = _doer_dict(command="zypper rm pkg", reboot="")
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("boom")
    with pytest.raises(RuntimeError, match="boom"):
        hg.perform_uninstall(["pkg"])
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_uninstall_transactional_triggers_reboot(mock_run):
    t1 = _stub_target("h1", transactional=True)
    t1.doer.return_value = _doer_dict(
        command="transactional-update -n pkg remove pkg",
        reboot="systemctl reboot",
    )
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_uninstall(["pkg"])
    # _reboot eventually triggers reconnect.
    t1.reconnect.assert_called_once_with(retry=10, backoff=True)


# ---------------------------------------------------------------------------
# perform_prepare
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_prepare_installs_all_packages_in_one_command(mock_run):
    """``perform_prepare`` installs every package in a SINGLE command (one
    transaction/snapshot), not one run per package -- the per-package loop left
    packages missing after reboot on transactional hosts."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = ""
    doer = _doer_dict(command="zypper in $package", reboot="")
    t1.doer.return_value = doer
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.perform_prepare(["pkg-a", "pkg-b"], MagicMock())
    per_pkg_calls = [
        c
        for c in mock_run.call_args_list
        if isinstance(c.args[1], dict) and "h1" in c.args[1]
    ]
    assert len(per_pkg_calls) == 1  # one combined call, not one-per-package
    doer["command"].substitute.assert_called_once_with(package="pkg-a pkg-b")
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_prepare_filters_branding_upstream(mock_run):
    """The hard-coded ``branding-upstream`` name is excluded from the combined
    install command."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = ""
    doer = _doer_dict(command="zypper in $package", reboot="")
    t1.doer.return_value = doer
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])  # type: ignore[arg-type]
    hg.perform_prepare(["pkg-a", "branding-upstream", "pkg-b"], MagicMock())
    doer["command"].substitute.assert_called_once_with(package="pkg-a pkg-b")


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_prepare_aborts_on_set_repo_error(mock_run, caplog):
    """``set_repo`` failure (non-empty ``lasterr``) logs critical and returns early."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = "repo failure"
    t1.lastin.return_value = "zypper ar"
    t1.lastout.return_value = ""
    t1.doer.return_value = _doer_dict(
        command="zypper in $package", reboot="", start_command=""
    )
    hg = HostsGroup([t1])
    with caplog.at_level("CRITICAL", logger="mtui.target.hostgroup"):
        hg.perform_prepare(["pkg-a"], MagicMock())
    assert any("Failed to prepare host" in r.message for r in caplog.records)
    # No per-package command should have been issued.
    per_pkg_calls = [
        c
        for c in mock_run.call_args_list
        if isinstance(c.args[1], dict) and "h1" in c.args[1]
    ]
    assert len(per_pkg_calls) == 0
    t1.unlock.assert_called()


# ---------------------------------------------------------------------------
# perform_downgrade
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_downgrade_picks_highest_version_then_runs(mock_run):
    """The list_command output is parsed and the newest version is chosen."""
    t1 = _stub_target("h1")
    # Pretend list_command output is parsed: ``pkg-a = 1.0`` and ``pkg-a = 1.5``.
    t1.lastout.return_value = "pkg-a = 1.0-1\npkg-a = 1.5-1\n"
    t1.doer.return_value = {
        **_doer_dict(reboot="", init_snapshot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": MagicMock(
            safe_substitute=MagicMock(return_value="zypper in pkg-a-1.5-1")
        ),
    }
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_downgrade(["pkg-a"], MagicMock())
    # Confirm the per-package command was substituted with the higher version.
    cmd_tpl = t1.doer.return_value["command"]
    cmd_tpl.safe_substitute.assert_any_call(package="pkg-a", version="1.5-1")
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_downgrade_host_without_versions_does_not_keyerror(mock_run):
    """A host whose list_command yields no versions is skipped, not a KeyError."""
    t1 = _stub_target("h1")
    t1.lastout.return_value = ""  # nothing parses -> no entry in `versions`
    t1.doer.return_value = {
        **_doer_dict(reboot="", init_snapshot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": MagicMock(safe_substitute=MagicMock(return_value="x")),
    }
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])
    # Must not raise KeyError('h1'); the package is simply skipped.
    hg.perform_downgrade(["pkg-a"], MagicMock())
    t1.doer.return_value["command"].safe_substitute.assert_not_called()
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_downgrade_missing_downgrader_does_not_lock(mock_run):
    """A missing downgrader returns early without leaving hosts locked."""
    from mtui.support.messages import MissingDowngraderError

    t1 = _stub_target("h1", transactional=True)
    t1.doer.side_effect = MissingDowngraderError("16")
    hg = HostsGroup([t1])
    hg.perform_downgrade(["pkg-a"], MagicMock())
    # The early return happens before update_lock(), so the host is never
    # locked (and therefore never stuck locked).
    t1.lock.assert_not_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_downgrade_unlocks_on_error(mock_run):
    t1 = _stub_target("h1")
    t1.lastout.return_value = ""
    t1.doer.return_value = {
        **_doer_dict(reboot="", init_snapshot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": MagicMock(safe_substitute=MagicMock(return_value="x")),
    }
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("downgrade boom")
    with pytest.raises(RuntimeError, match="downgrade boom"):
        hg.perform_downgrade(["pkg-a"], MagicMock())
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_downgrade_transactional_combines_packages(mock_run):
    """A transactional host downgrades EVERY package in a SINGLE command (one
    transaction/snapshot): the per-package loop opened a snapshot per package and
    they never landed together, so the packages stayed at the test version after
    reboot. The combined call must substitute the full ``name=version`` spec list
    once, and the downgrader check must run for the transactional host."""
    t1 = _stub_target("h1", transactional=True)
    # list_command output parsed into versions: pkg-a -> 1.5-1, pkg-b -> 2.0-1.
    t1.lastout.return_value = "pkg-a = 1.5-1\npkg-b = 2.0-1\n"
    command = MagicMock(safe_substitute=MagicMock(return_value="tu pkg in ..."))
    t1.doer.return_value = {
        **_doer_dict(reboot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": command,
    }
    t1.check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_downgrade(["pkg-a", "pkg-b"], MagicMock())
    # Every spec joined into ONE invocation, not one call per package.
    command.safe_substitute.assert_called_once_with(package="pkg-a=1.5-1 pkg-b=2.0-1")
    t1.check.assert_called_with("downgrader")
    t1.unlock.assert_called()


# ---------------------------------------------------------------------------
# perform_update
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_runs_full_flow_with_noprepare_and_noscript(mock_run):
    """``noprepare`` skips prepare; ``noscript`` skips Pre/Post/Compare scripts."""
    t1 = _stub_target("h1")
    t1.packages = {}  # short-circuit package_check
    t1.doer.return_value = _doer_dict(command="zypper up", reboot="")
    t1.check.return_value = MagicMock()
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    hg.perform_update(testreport, ["noprepare", "noscript"])
    # Pre/Post/Compare scripts must not have been run.
    testreport.run_scripts.assert_not_called()
    t1.unlock.assert_called()


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_runs_pre_post_and_compare_scripts(mock_run):
    """Default flow runs Pre, Post and Compare scripts when not in auto mode."""
    from mtui.update_workflow.hooks import CompareScript, PostScript, PreScript

    t1 = _stub_target("h1")
    t1.packages = {}
    # perform_update internally calls perform_prepare → two distinct
    # doer roles are looked up against the same target. Dispatch by role.
    doers = {
        "updater": _doer_dict(command="zypper up", reboot=""),
        "preparer": _doer_dict(
            command="zypper in $package", reboot="", start_command=""
        ),
    }
    t1.doer.side_effect = lambda role, *a, **kw: doers[role]
    t1.check.return_value = MagicMock()
    t1.lasterr.return_value = ""
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    hg.perform_update(testreport, [])
    # PreScript before update, Post + Compare after.
    script_classes_called = [c.args[0] for c in testreport.run_scripts.call_args_list]
    assert PreScript in script_classes_called
    assert PostScript in script_classes_called
    assert CompareScript in script_classes_called


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_unlocks_when_run_fails(mock_run):
    t1 = _stub_target("h1")
    t1.packages = {}
    t1.doer.return_value = _doer_dict(command="zypper up", reboot="")
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("update boom")
    with pytest.raises(RuntimeError, match="update boom"):
        hg.perform_update(testreport, ["noprepare", "noscript"])
    t1.unlock.assert_called()


@patch.object(HostsGroup, "_reboot")
@patch.object(HostsGroup, "_fanout_set_repo")
@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_reports_all_host_failures(
    mock_run, mock_fanout, mock_reboot, caplog
):
    """Every host is checked and reported; the raised error names all failed
    hosts, not just the first, and no reboot happens on failure."""
    t1 = _stub_target("h1")
    t1.packages = {}
    t2 = _stub_target("h2")
    t2.packages = {}
    for t, host in ((t1, "h1"), (t2, "h2")):
        t.doer.return_value = _doer_dict(command="zypper up", reboot="")
        t.check.return_value = MagicMock(
            side_effect=UpdateError("Dependency Error", host)
        )
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1, t2])

    with (
        caplog.at_level(logging.ERROR, logger="mtui.target.hostgroup"),
        pytest.raises(UpdateError) as ei,
    ):
        hg.perform_update(testreport, ["noprepare", "noscript"])

    msg = str(ei.value)
    assert "h1" in msg  # aggregated, not just the first
    assert "h2" in msg
    logged = " ".join(r.message for r in caplog.records)
    assert "h1" in logged  # both logged per-host
    assert "h2" in logged
    mock_reboot.assert_not_called()  # no reboot on failure
    t1.unlock.assert_called()
    t2.unlock.assert_called()


@patch.object(HostsGroup, "_reboot")
@patch.object(HostsGroup, "_fanout_set_repo")
@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_single_failure_reraises_original(
    mock_run, mock_fanout, mock_reboot
):
    """A single failing host re-raises that host's original UpdateError unchanged
    (backward compatible); the passing host is not reported as failed."""
    t1 = _stub_target("h1")
    t1.packages = {}
    t2 = _stub_target("h2")
    t2.packages = {}
    t1.doer.return_value = _doer_dict(command="zypper up", reboot="")
    t2.doer.return_value = _doer_dict(command="zypper up", reboot="")
    orig = UpdateError("Dependency Error", "h1")
    t1.check.return_value = MagicMock(side_effect=orig)
    t2.check.return_value = MagicMock()  # h2 passes
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1, t2])

    with pytest.raises(UpdateError) as ei:
        hg.perform_update(testreport, ["noprepare", "noscript"])

    assert ei.value is orig  # exact original error, unchanged
    assert str(ei.value) == "h1: Dependency Error"
    mock_reboot.assert_not_called()


@patch.object(HostsGroup, "_fanout_set_repo")
@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_removes_repos_at_end(mock_run, mock_fanout):
    """``perform_update`` fans out set_repo('add') first, then ('remove')."""
    t1 = _stub_target("h1")
    t1.packages = {}
    t1.doer.return_value = _doer_dict(command="zypper up", reboot="")
    t1.check.return_value = MagicMock()
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    hg.perform_update(testreport, ["noprepare", "noscript"])

    operations = [c.args[0] for c in mock_fanout.call_args_list]
    # The repo lifecycle is: add at the start, remove at the end.
    assert operations[0] == "add"
    assert operations[-1] == "remove"


@patch.object(HostsGroup, "_fanout_set_repo")
@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_perform_update_keeps_repos_when_run_fails(mock_run, mock_fanout):
    """On a failed update the test repos are KEPT (not stripped) so the host
    can be retried/diagnosed without re-adding them."""
    t1 = _stub_target("h1")
    t1.packages = {}
    t1.doer.return_value = _doer_dict(command="zypper up", reboot="")
    testreport = MagicMock()
    testreport.workflow = Workflow.MANUAL
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("update boom")

    with pytest.raises(RuntimeError, match="update boom"):
        hg.perform_update(testreport, ["noprepare", "noscript"])

    operations = [c.args[0] for c in mock_fanout.call_args_list]
    # Repo was added, but NOT removed on failure.
    assert operations == ["add"]
    assert "remove" not in operations
    # The host is still unlocked despite keeping the repo.
    t1.unlock.assert_called()


# ---------------------------------------------------------------------------
# remaining group report methods
# ---------------------------------------------------------------------------


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_report_history_with_events_uses_grep(mock_run):
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_history(sink, count=5, events=["state", "comment"])
    cmd = mock_run.call_args[0][1]
    assert "grep" in cmd
    assert "-m 5" in cmd
    t1.reporter.history.assert_called_once_with(sink)


@patch("mtui.hosts.target.hostgroup.RunCommand")
def test_report_history_without_events_uses_tail(mock_run):
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_history(sink, count=10, events=[])
    cmd = mock_run.call_args[0][1]
    assert "tail -n 10" in cmd
    t1.reporter.history.assert_called_once_with(sink)


def test_report_sessions_delegates():
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_sessions(sink)
    t1.reporter.sessions.assert_called_once_with(sink)


def test_report_log_delegates():
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_log(sink, "arg")
    t1.reporter.log.assert_called_once_with(sink, "arg")


# --- Interactive flag propagation (MCP spinner suppression) ---


def test_hostgroup_defaults_to_interactive():
    """The REPL path constructs ``HostsGroup`` without the kwarg."""
    hg = HostsGroup([])
    assert hg.interactive is True


def test_hostgroup_non_interactive_select_inherits_flag():
    """A sub-group from ``select`` must carry the parent's ``interactive`` bit."""
    t1 = _stub_target("h1")
    t2 = _stub_target("h2")
    t1.state = "enabled"
    t2.state = "enabled"
    hg = HostsGroup([t1, t2], interactive=False)

    sub_all = hg.select()
    sub_named = hg.select(["h1"])
    sub_enabled = hg.select(enabled=True)

    assert sub_all.interactive is False
    assert sub_named.interactive is False
    assert sub_enabled.interactive is False


def test_hostgroup_non_interactive_passes_desc_none_to_run_parallel():
    """``_fanout_set_repo`` on a headless group must pass ``desc=None``."""
    t1 = _stub_target("h1")
    hg = HostsGroup([t1], interactive=False)

    with patch("mtui.hosts.target.hostgroup.run_parallel") as mock_rp:
        hg._fanout_set_repo("add", MagicMock())  # noqa: SLF001

    assert mock_rp.call_args.kwargs["desc"] is None


def test_hostgroup_interactive_passes_desc_label_to_run_parallel():
    """The REPL path keeps the descriptive spinner label."""
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])  # interactive=True by default

    with patch("mtui.hosts.target.hostgroup.run_parallel") as mock_rp:
        hg._fanout_set_repo("add", MagicMock())  # noqa: SLF001

    assert mock_rp.call_args.kwargs["desc"] == "set_repo add"


def test_hostgroup_non_interactive_run_passes_through_to_runcommand():
    """``HostsGroup.run`` forwards ``interactive`` into ``RunCommand``."""
    from mtui.types import ExecutionMode

    t1 = _stub_target("h1")
    t1.mode = ExecutionMode.PARALLEL
    hg = HostsGroup([t1], interactive=False)

    with patch("mtui.hosts.target.hostgroup.RunCommand") as mock_rc:
        hg.run("true")

    assert mock_rc.call_args.kwargs["interactive"] is False


def test_hostgroup_non_interactive_sftp_remove_forwards_flag():
    """``sftp_remove`` constructs ``FileDelete(interactive=False)``."""
    t1 = _stub_target("h1")
    hg = HostsGroup([t1], interactive=False)

    with patch("mtui.hosts.target.hostgroup.FileDelete") as mock_fd:
        hg.sftp_remove(Path("/tmp/x"))

    assert mock_fd.call_args.kwargs["interactive"] is False
