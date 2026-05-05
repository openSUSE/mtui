"""Tests for the mtui hostgroup module."""

from pathlib import Path
from unittest.mock import MagicMock, PropertyMock, patch

import pytest

from mtui.exceptions import UpdateError
from mtui.messages import HostIsNotConnectedError
from mtui.target.hostgroup import HostsGroup
from mtui.target.locks import TargetLockedError


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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]

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
    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]

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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    selected = hg.select(["h1"])

    assert len(selected) == 1
    assert "h1" in selected


def test_hostgroup_select_nonexistent_host_raises():
    """Test select() with unknown hostname raises HostIsNotConnectedError."""
    t1 = MagicMock()
    t1.hostname = "h1"
    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]

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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.unlock("test_comment")

    t1.unlock.assert_called_once_with("test_comment")
    t2.unlock.assert_called_once_with("test_comment")


def test_hostgroup_unlock_suppresses_target_locked_error():
    """Test unlock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.unlock.side_effect = TargetLockedError("locked by someone")

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.unlock()  # should not raise


def test_hostgroup_lock_delegates():
    """Test lock() calls lock on every target."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.lock("comment")

    t1.lock.assert_called_once_with("comment")
    t2.lock.assert_called_once_with("comment")


def test_hostgroup_lock_suppresses_target_locked_error():
    """Test lock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.lock.side_effect = TargetLockedError("locked")

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
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

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    result = hg.query_versions(["pkg"])

    assert len(result) == 2
    t1.query_package_versions.assert_called_once_with(["pkg"])
    t2.query_package_versions.assert_called_once_with(["pkg"])


def test_hostgroup_add_history():
    """Test add_history delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.add_history("data")

    t1.add_history.assert_called_once_with("data")


def test_hostgroup_names():
    """Test names() returns all hostnames."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "alpha"
    t2.hostname = "beta"

    hg = HostsGroup([t1, t2])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    names = hg.names()

    assert set(names) == {"alpha", "beta"}


# --- update_lock ---


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
def test_update_lock_locks_unlocked_hosts(mock_queue, mock_thread_cls):
    """Test update_lock() locks hosts that are not locked."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.update_lock()

    t1.lock.assert_called_once()


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
def test_update_lock_raises_when_locked_by_other(mock_queue, mock_thread_cls):
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

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]

    with pytest.raises(UpdateError, match="Hosts locked"):
        hg.update_lock()


# --- perform_install ---


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_install_runs_and_unlocks(mock_run, mock_queue, mock_thread):
    """Test perform_install runs commands and always unlocks."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False
    t1.transactional = False
    t1.get_installer.return_value = {
        "command": MagicMock(substitute=MagicMock(return_value="zypper in pkg")),
        "reboot": MagicMock(substitute=MagicMock(return_value="")),
    }
    t1.get_installer_check.return_value = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.perform_install(["pkg"])

    # Verify unlock was called (cleanup always happens)
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_install_unlocks_on_error(mock_run, mock_queue, mock_thread):
    """Test perform_install unlocks even when commands raise."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.is_locked.return_value = False
    t1.transactional = False
    t1.get_installer.return_value = {
        "command": MagicMock(substitute=MagicMock(return_value="zypper in pkg")),
        "reboot": MagicMock(substitute=MagicMock(return_value="")),
    }

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    # Make run raise
    mock_run.return_value.run.side_effect = RuntimeError("connection lost")

    with pytest.raises(RuntimeError, match="connection lost"):
        hg.perform_install(["pkg"])

    # unlock must still be called
    t1.unlock.assert_called()


# --- Report methods ---


def test_report_self_delegates():
    """Test report_self delegates to each target with a sink."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.report_self(sink)

    t1.report_self.assert_called_once_with(sink)


def test_report_locks_delegates():
    """Test report_locks delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.report_locks(sink)

    t1.report_locks.assert_called_once_with(sink)


def test_report_timeout_delegates():
    """Test report_timeout delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.report_timeout(sink)

    t1.report_timeout.assert_called_once_with(sink)


def test_report_products_delegates():
    """Test report_products delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    hg.report_products(sink)

    t1.report_products.assert_called_once_with(sink)


# ---------------------------------------------------------------------------
# Group-level sftp wrappers
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.FileDownload")
def test_sftp_get_delegates_to_filedownload(mock_dl):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_get(Path("/remote/x"), Path("/local/x"))
    mock_dl.assert_called_once()
    mock_dl.return_value.run.assert_called_once()


@patch("mtui.target.hostgroup.FileUpload")
def test_sftp_put_delegates_to_fileupload(mock_ul):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_put(Path("/local/x"), Path("/remote/x"))
    mock_ul.assert_called_once()
    mock_ul.return_value.run.assert_called_once()


@patch("mtui.target.hostgroup.FileDelete")
def test_sftp_remove_delegates_to_filedelete(mock_rm):
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg.sftp_remove(Path("/remote/x"))
    mock_rm.assert_called_once()
    mock_rm.return_value.run.assert_called_once()


# ---------------------------------------------------------------------------
# _reboot
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.RunCommand")
def test_reboot_runs_commands_and_reconnects(mock_run):
    """Non-empty reboot dict triggers run + per-host reconnect."""
    t1 = _stub_target("h1", transactional=True)
    hg = HostsGroup([t1])
    hg._reboot({"h1": "systemctl reboot"})
    mock_run.return_value.run.assert_called_once()
    t1.reconnect.assert_called_once_with(retry=10, backoff=True)


def test_reboot_empty_dict_is_noop():
    t1 = _stub_target("h1")
    hg = HostsGroup([t1])
    hg._reboot({})
    t1.reconnect.assert_not_called()


# ---------------------------------------------------------------------------
# update_lock — comment-logging branch
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
def test_update_lock_logs_other_user_comment(mock_queue, mock_thread, caplog):
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


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_uninstall_runs_and_unlocks(mock_run, mock_queue, mock_thread):
    t1 = _stub_target("h1")
    t1.get_uninstaller.return_value = _doer_dict(command="zypper rm pkg", reboot="")
    t1.get_uninstaller_check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_uninstall(["pkg"])
    mock_run.assert_called()
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_uninstall_unlocks_on_error(mock_run, mock_queue, mock_thread):
    t1 = _stub_target("h1")
    t1.get_uninstaller.return_value = _doer_dict(command="zypper rm pkg", reboot="")
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("boom")
    with pytest.raises(RuntimeError, match="boom"):
        hg.perform_uninstall(["pkg"])
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_uninstall_transactional_triggers_reboot(
    mock_run, mock_queue, mock_thread
):
    t1 = _stub_target("h1", transactional=True)
    t1.get_uninstaller.return_value = _doer_dict(
        command="transactional-update -n pkg remove pkg",
        reboot="systemctl reboot",
    )
    t1.get_uninstaller_check.return_value = MagicMock()
    hg = HostsGroup([t1])
    hg.perform_uninstall(["pkg"])
    # _reboot eventually triggers reconnect.
    t1.reconnect.assert_called_once_with(retry=10, backoff=True)


# ---------------------------------------------------------------------------
# perform_prepare
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_prepare_runs_per_package_command(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    """``perform_prepare`` runs the command-template once per package."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = ""
    t1.get_preparer.return_value = _doer_dict(
        command="zypper in $package", reboot="", start_command=""
    )
    t1.get_preparer_check.return_value = MagicMock()
    # ``queue.unfinished_tasks`` is used as a loop sentinel; force it to 0.
    mock_queue.unfinished_tasks = 0
    hg = HostsGroup([t1])
    hg.perform_prepare(["pkg-a", "pkg-b"], MagicMock())
    # 2 packages → 2 RunCommand invocations beyond the lock cleanup.
    assert mock_run.call_count >= 2
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_prepare_filters_branding_upstream(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    """The hard-coded ``branding-upstream`` name is excluded from per-pkg cmds."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = ""
    t1.get_preparer.return_value = _doer_dict(
        command="zypper in $package", reboot="", start_command=""
    )
    t1.get_preparer_check.return_value = MagicMock()
    mock_queue.unfinished_tasks = 0
    hg = HostsGroup([t1])
    hg.perform_prepare(["pkg-a", "branding-upstream", "pkg-b"], MagicMock())
    # 2 surviving packages.
    per_pkg_calls = [
        c
        for c in mock_run.call_args_list
        if isinstance(c.args[1], dict) and "h1" in c.args[1]
    ]
    assert len(per_pkg_calls) == 2


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_prepare_aborts_on_set_repo_error(
    mock_run, mock_queue, mock_thread, mock_spinner, caplog
):
    """``set_repo`` failure (non-empty ``lasterr``) logs critical and returns early."""
    t1 = _stub_target("h1")
    t1.lasterr.return_value = "repo failure"
    t1.lastin.return_value = "zypper ar"
    t1.lastout.return_value = ""
    t1.get_preparer.return_value = _doer_dict(
        command="zypper in $package", reboot="", start_command=""
    )
    mock_queue.unfinished_tasks = 0
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


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_downgrade_picks_highest_version_then_runs(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    """The list_command output is parsed and the newest version is chosen."""
    t1 = _stub_target("h1")
    # Pretend list_command output is parsed: ``pkg-a = 1.0`` and ``pkg-a = 1.5``.
    t1.lastout.return_value = "pkg-a = 1.0-1\npkg-a = 1.5-1\n"
    t1.get_downgrader.return_value = {
        **_doer_dict(reboot="", init_snapshot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": MagicMock(
            safe_substitute=MagicMock(return_value="zypper in pkg-a-1.5-1")
        ),
    }
    t1.get_downgrader_check.return_value = MagicMock()
    mock_queue.unfinished_tasks = 0
    hg = HostsGroup([t1])
    hg.perform_downgrade(["pkg-a"], MagicMock())
    # Confirm the per-package command was substituted with the higher version.
    cmd_tpl = t1.get_downgrader.return_value["command"]
    cmd_tpl.safe_substitute.assert_any_call(package="pkg-a", version="1.5-1")
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_downgrade_unlocks_on_error(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    t1 = _stub_target("h1")
    t1.lastout.return_value = ""
    t1.get_downgrader.return_value = {
        **_doer_dict(reboot="", init_snapshot=""),
        "list_command": MagicMock(safe_substitute=MagicMock(return_value="rpm -qa")),
        "command": MagicMock(safe_substitute=MagicMock(return_value="x")),
    }
    mock_queue.unfinished_tasks = 0
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("downgrade boom")
    with pytest.raises(RuntimeError, match="downgrade boom"):
        hg.perform_downgrade(["pkg-a"], MagicMock())
    t1.unlock.assert_called()


# ---------------------------------------------------------------------------
# perform_update
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_update_runs_full_flow_with_noprepare_and_noscript(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    """``noprepare`` skips prepare; ``noscript`` skips Pre/Post/Compare scripts."""
    t1 = _stub_target("h1")
    t1.packages = {}  # short-circuit package_check
    t1.get_updater.return_value = _doer_dict(command="zypper up", reboot="")
    t1.get_updater_check.return_value = MagicMock()
    mock_queue.unfinished_tasks = 0
    testreport = MagicMock()
    testreport.config.auto = False
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    hg.perform_update(testreport, ["noprepare", "noscript"])
    # Pre/Post/Compare scripts must not have been run.
    testreport.run_scripts.assert_not_called()
    t1.unlock.assert_called()


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_update_runs_pre_post_and_compare_scripts(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    """Default flow runs Pre, Post and Compare scripts when not in auto mode."""
    from mtui.hooks import CompareScript, PostScript, PreScript

    t1 = _stub_target("h1")
    t1.packages = {}
    t1.get_updater.return_value = _doer_dict(command="zypper up", reboot="")
    t1.get_updater_check.return_value = MagicMock()
    t1.get_preparer.return_value = _doer_dict(
        command="zypper in $package", reboot="", start_command=""
    )
    t1.get_preparer_check.return_value = MagicMock()
    t1.lasterr.return_value = ""
    mock_queue.unfinished_tasks = 0
    testreport = MagicMock()
    testreport.config.auto = False
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


@patch("mtui.target.hostgroup.spinner")
@patch("mtui.target.hostgroup.ThreadedMethod")
@patch("mtui.target.hostgroup.queue")
@patch("mtui.target.hostgroup.RunCommand")
def test_perform_update_unlocks_when_run_fails(
    mock_run, mock_queue, mock_thread, mock_spinner
):
    t1 = _stub_target("h1")
    t1.packages = {}
    t1.get_updater.return_value = _doer_dict(command="zypper up", reboot="")
    mock_queue.unfinished_tasks = 0
    testreport = MagicMock()
    testreport.config.auto = False
    testreport.get_package_list.return_value = ["pkg"]
    testreport.rrid.maintenance_id = "1"
    testreport.rrid.review_id = "2"
    hg = HostsGroup([t1])
    mock_run.return_value.run.side_effect = RuntimeError("update boom")
    with pytest.raises(RuntimeError, match="update boom"):
        hg.perform_update(testreport, ["noprepare", "noscript"])
    t1.unlock.assert_called()


# ---------------------------------------------------------------------------
# remaining group report methods
# ---------------------------------------------------------------------------


@patch("mtui.target.hostgroup.RunCommand")
def test_report_history_with_events_uses_grep(mock_run):
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_history(sink, count=5, events=["state", "comment"])
    cmd = mock_run.call_args[0][1]
    assert "grep" in cmd
    assert "-m 5" in cmd
    t1.report_history.assert_called_once_with(sink)


@patch("mtui.target.hostgroup.RunCommand")
def test_report_history_without_events_uses_tail(mock_run):
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_history(sink, count=10, events=[])
    cmd = mock_run.call_args[0][1]
    assert "tail -n 10" in cmd
    t1.report_history.assert_called_once_with(sink)


def test_report_sessions_delegates():
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_sessions(sink)
    t1.report_sessions.assert_called_once_with(sink)


def test_report_log_delegates():
    t1 = _stub_target("h1")
    sink = MagicMock()
    hg = HostsGroup([t1])
    hg.report_log(sink, "arg")
    t1.report_log.assert_called_once_with(sink, "arg")
