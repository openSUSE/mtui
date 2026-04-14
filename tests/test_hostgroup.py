"""Tests for the mtui hostgroup module."""

from unittest.mock import MagicMock, PropertyMock, patch

import pytest

from mtui.exceptions import UpdateError
from mtui.messages import HostIsNotConnectedError
from mtui.target.hostgroup import HostsGroup
from mtui.target.locks import TargetLockedError


# --- Initialization and selection ---


def test_hostgroup_init():
    """Test HostsGroup initialization creates correct mapping."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "host1.example.com"
    t2.hostname = "host2.example.com"

    hg = HostsGroup([t1, t2])

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
    hg = HostsGroup([t1])

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

    hg = HostsGroup([t1, t2])
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

    hg = HostsGroup([t1, t2])
    selected = hg.select(["h1"])

    assert len(selected) == 1
    assert "h1" in selected


def test_hostgroup_select_nonexistent_host_raises():
    """Test select() with unknown hostname raises HostIsNotConnectedError."""
    t1 = MagicMock()
    t1.hostname = "h1"
    hg = HostsGroup([t1])

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

    hg = HostsGroup([t1, t2])
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

    hg = HostsGroup([t1, t2])
    hg.unlock("test_comment")

    t1.unlock.assert_called_once_with("test_comment")
    t2.unlock.assert_called_once_with("test_comment")


def test_hostgroup_unlock_suppresses_target_locked_error():
    """Test unlock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.unlock.side_effect = TargetLockedError("locked by someone")

    hg = HostsGroup([t1])
    hg.unlock()  # should not raise


def test_hostgroup_lock_delegates():
    """Test lock() calls lock on every target."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "h1"
    t2.hostname = "h2"

    hg = HostsGroup([t1, t2])
    hg.lock("comment")

    t1.lock.assert_called_once_with("comment")
    t2.lock.assert_called_once_with("comment")


def test_hostgroup_lock_suppresses_target_locked_error():
    """Test lock() suppresses TargetLockedError from individual targets."""
    t1 = MagicMock()
    t1.hostname = "h1"
    t1.lock.side_effect = TargetLockedError("locked")

    hg = HostsGroup([t1])
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

    hg = HostsGroup([t1, t2])
    result = hg.query_versions(["pkg"])

    assert len(result) == 2
    t1.query_package_versions.assert_called_once_with(["pkg"])
    t2.query_package_versions.assert_called_once_with(["pkg"])


def test_hostgroup_add_history():
    """Test add_history delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"

    hg = HostsGroup([t1])
    hg.add_history("data")

    t1.add_history.assert_called_once_with("data")


def test_hostgroup_names():
    """Test names() returns all hostnames."""
    t1 = MagicMock()
    t2 = MagicMock()
    t1.hostname = "alpha"
    t2.hostname = "beta"

    hg = HostsGroup([t1, t2])
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

    hg = HostsGroup([t1])
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

    hg = HostsGroup([t1])

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

    hg = HostsGroup([t1])
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

    hg = HostsGroup([t1])
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

    hg = HostsGroup([t1])
    hg.report_self(sink)

    t1.report_self.assert_called_once_with(sink)


def test_report_locks_delegates():
    """Test report_locks delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])
    hg.report_locks(sink)

    t1.report_locks.assert_called_once_with(sink)


def test_report_timeout_delegates():
    """Test report_timeout delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])
    hg.report_timeout(sink)

    t1.report_timeout.assert_called_once_with(sink)


def test_report_products_delegates():
    """Test report_products delegates to each target."""
    t1 = MagicMock()
    t1.hostname = "h1"
    sink = MagicMock()

    hg = HostsGroup([t1])
    hg.report_products(sink)

    t1.report_products.assert_called_once_with(sink)
