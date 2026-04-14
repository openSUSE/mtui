"""Tests for the mtui target module."""

from unittest.mock import MagicMock

from mtui.target import Target
from mtui.types import HostLog, Package
from mtui.types.product import Product
from mtui.types.rpmver import RPMVersion

# --- Initialization ---


def test_target_init_defaults(mock_config):
    """Test Target initialization with default parameters."""
    target = Target(mock_config, "test-host.example.com")

    assert target.config is mock_config
    assert target.host == "test-host.example.com"
    assert target.hostname == "test-host.example.com"
    assert target.port == ""
    assert target.state == "enabled"
    assert target._timeout == 300
    assert target.exclusive is False
    assert target.transactional is False
    assert target.packages == {}


def test_target_init_with_port(mock_config):
    """Test Target initialization with port in hostname."""
    target = Target(mock_config, "test-host.example.com:2222")

    assert target.host == "test-host.example.com"
    assert target.port == "2222"
    assert target.hostname == "test-host.example.com:2222"


def test_target_init_with_packages(mock_config):
    """Test Target initialization with packages dict."""
    packages = {"standard": {"bash": "5.1-1.2"}}
    target = Target(mock_config, "host.example.com", packages)

    assert target._pkgs == packages


def test_target_init_with_state(mock_config):
    """Test Target initialization with different states."""
    for state in ("enabled", "disabled", "serial", "parallel"):
        target = Target(mock_config, "host.example.com", state=state)
        assert target.state == state


def test_target_init_with_timeout(mock_config):
    """Test Target initialization with custom timeout."""
    target = Target(mock_config, "host.example.com", timeout=600)
    assert target._timeout == 600


def test_target_init_with_exclusive(mock_config):
    """Test Target initialization with exclusive mode."""
    target = Target(mock_config, "host.example.com", exclusive=True)
    assert target.exclusive is True


def test_target_init_custom_classes(mock_config):
    """Test Target initialization with custom lock and connection classes."""
    mock_lock_class = MagicMock()
    mock_conn_class = MagicMock()

    target = Target(
        mock_config,
        "host.example.com",
        lock=mock_lock_class,
        connection=mock_conn_class,
    )

    assert target.TargetLock is mock_lock_class
    assert target.Connection is mock_conn_class


# --- String representation ---


def test_target_repr(mock_config):
    """Test Target __repr__."""
    target = Target(mock_config, "host.example.com")
    assert "Target" in repr(target)
    assert "host.example.com" in repr(target)


def test_target_str(mock_config):
    """Test Target __str__ returns hostname."""
    target = Target(mock_config, "host.example.com")
    assert str(target) == "host.example.com"


# --- last* methods ---


def test_last_methods_empty(mock_config):
    """Test last* methods return empty strings when no output."""
    target = Target(mock_config, "host.example.com")
    assert target.lastin() == ""
    assert target.lastout() == ""
    assert target.lasterr() == ""
    assert target.lastexit() == ""


def test_last_methods_with_output(mock_config):
    """Test last* methods after appending output."""
    target = Target(mock_config, "host.example.com")
    target.out = HostLog()
    target.out.append(["ls -la", "file1\nfile2\n", "warning\n", 0, 5])

    assert target.lastin() == "ls -la"
    assert "file1" in target.lastout()
    assert "warning" in target.lasterr()
    assert target.lastexit() == 0


# --- lock/unlock ---


def test_target_lock_delegates(mock_config):
    """Test lock() delegates to _lock."""
    target = Target(mock_config, "host.example.com")
    target._lock = MagicMock()

    target.lock("test comment")
    target._lock.lock.assert_called_once_with("test comment")


def test_target_unlock_delegates(mock_config):
    """Test unlock() delegates to _lock."""
    target = Target(mock_config, "host.example.com")
    target._lock = MagicMock()

    target.unlock()
    target._lock.unlock.assert_called_once_with(False)


def test_target_unlock_with_force(mock_config):
    """Test unlock(force=True) passes force flag."""
    target = Target(mock_config, "host.example.com")
    target._lock = MagicMock()

    target.unlock(force=True)
    target._lock.unlock.assert_called_once_with(True)


# --- run() state machine ---


def test_run_enabled_executes_command(mock_config):
    """Test run() in enabled state executes the command."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.run.return_value = 0
    target.connection.stdout = "output"
    target.connection.stderr = ""
    target.state = "enabled"

    target.run("echo hello")

    target.connection.run.assert_called_once_with("echo hello", None)
    assert target.lastout() == "output"


def test_run_dryrun_does_not_execute(mock_config):
    """Test run() in dryrun state does not execute command."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.state = "dryrun"

    target.run("rm -rf /")

    target.connection.run.assert_not_called()
    assert "dryrun" in target.lastout()


def test_run_disabled_does_not_execute(mock_config):
    """Test run() in disabled state does not execute command."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.state = "disabled"

    target.run("some command")

    target.connection.run.assert_not_called()


def test_run_handles_command_timeout(mock_config):
    """Test run() catches CommandTimeoutError and sets exit code to -1."""
    from mtui.connection import CommandTimeoutError

    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.run.side_effect = CommandTimeoutError("echo hello")
    target.state = "enabled"

    target.run("echo hello")

    # Should not raise; exit code should be -1
    assert target.lastexit() == -1


def test_run_handles_generic_exception(mock_config):
    """Test run() catches generic exceptions and sets exit code to -1."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.run.side_effect = OSError("connection lost")
    target.state = "enabled"

    target.run("echo hello")

    assert target.lastexit() == -1


# --- reconnect ---


def test_reconnect_delegates(mock_config):
    """Test reconnect() delegates to connection."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()

    target.reconnect(3, True)

    target.connection.reconnect.assert_called_once_with(3, True)


# --- set_timeout ---


def test_set_timeout(mock_config):
    """Test set_timeout updates both connection and internal timeout."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()

    target.set_timeout(600)

    assert target._timeout == 600
    assert target.connection.timeout == 600


# --- close ---


def test_close_unlocks_and_closes(mock_config):
    """Test close() unlocks and closes the connection."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.is_active.return_value = True
    target._lock = MagicMock()

    target.close()

    target._lock.unlock.assert_called_once_with(False)
    target.connection.close.assert_called_once()


def test_close_with_reboot(mock_config):
    """Test close() with reboot action."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.is_active.return_value = True
    target.connection.run.return_value = 0
    target.connection.stdout = ""
    target.connection.stderr = ""
    target._lock = MagicMock()
    target.state = "enabled"

    target.close(action="reboot")

    target.connection.close.assert_called_once()


def test_close_handles_lost_connection(mock_config):
    """Test close() handles lost connections gracefully."""
    target = Target(mock_config, "host.example.com")
    target.connection = MagicMock()
    target.connection.is_active.side_effect = Exception("connection lost")

    target.close()  # should not raise

    target.connection.close.assert_called_once()


# --- _parse_packages ---


def test_parse_packages_standard(mock_config):
    """Test _parse_packages with 'standard' key."""
    target = Target(mock_config, "host.example.com")
    target.system = MagicMock()
    target.system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    target._pkgs = {"standard": {"bash": "5.1-1.2", "openssl": "3.0.8-1.2"}}

    result = target._parse_packages()

    assert "bash" in result
    assert "openssl" in result
    assert isinstance(result["bash"], Package)
    assert result["bash"].required == RPMVersion("5.1-1.2")


def test_parse_packages_by_version(mock_config):
    """Test _parse_packages matches on base version."""
    target = Target(mock_config, "host.example.com")
    target.system = MagicMock()
    target.system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    target._pkgs = {
        "15-SP5": {"bash": "5.1-1.2"},
        "12-SP5": {"bash": "4.3-1.0"},
    }

    result = target._parse_packages()

    assert "bash" in result
    assert result["bash"].required == RPMVersion("5.1-1.2")


def test_parse_packages_none(mock_config):
    """Test _parse_packages with no packages."""
    target = Target(mock_config, "host.example.com")
    target.system = MagicMock()
    target.system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    target._pkgs = None

    result = target._parse_packages()
    assert result == {}


# --- report methods with assertions ---


def test_report_self_calls_sink(mock_target):
    """Test report_self calls sink with correct args."""
    sink = MagicMock()
    mock_target.report_self(sink)

    sink.assert_called_once_with(
        mock_target.hostname,
        mock_target.system,
        mock_target.transactional,
        mock_target.state,
        mock_target.exclusive,
    )


def test_report_history_calls_sink(mock_target):
    """Test report_history calls sink with correct args."""
    sink = MagicMock()
    # Need output to split
    mock_target.out = HostLog()
    mock_target.out.append(["cmd", "line1\nline2", "", 0, 0])

    mock_target.report_history(sink)

    sink.assert_called_once()
    args = sink.call_args[0]
    assert args[0] == mock_target.hostname


def test_report_timeout_calls_sink(mock_target):
    """Test report_timeout calls sink with timeout."""
    sink = MagicMock()
    mock_target.report_timeout(sink)

    sink.assert_called_once()
    args = sink.call_args[0]
    assert args[0] == mock_target.hostname


# --- sftp delegation ---


def test_sftp_put_enabled(mock_target):
    """Test sftp_put in enabled state delegates to connection."""
    from pathlib import Path

    mock_target.state = "enabled"
    mock_target.sftp_put(Path("/local/file"), Path("/remote/file"))

    mock_target.connection.sftp_put.assert_called_once()


def test_sftp_put_dryrun(mock_target):
    """Test sftp_put in dryrun state does not transfer."""
    from pathlib import Path

    mock_target.state = "dryrun"
    mock_target.sftp_put(Path("/local/file"), Path("/remote/file"))

    mock_target.connection.sftp_put.assert_not_called()
