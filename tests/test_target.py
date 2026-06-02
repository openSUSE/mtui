"""Tests for the mtui target module."""

import errno
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mtui.hosts.target import Target, TargetLockedError
from mtui.types import ExecutionMode, HostLog, Package
from mtui.types.product import Product
from mtui.types.rpmver import RPMVersion

# --- Initialization ---


def _check_defaults(mock_config, target):
    assert target.config is mock_config
    assert target.host == "test-host.example.com"
    assert target.hostname == "test-host.example.com"
    assert target.port == ""
    assert target.state == "enabled"
    assert target._timeout == 300
    assert target.mode is ExecutionMode.PARALLEL
    assert target.transactional is False
    assert target.packages == {}


def _check_port(_, target):
    assert target.host == "test-host.example.com"
    assert target.port == "2222"
    assert target.hostname == "test-host.example.com:2222"


def _check_packages(_, target):
    assert target._pkgs == {"standard": {"bash": "5.1-1.2"}}


def _check_state(_, target):
    # The parameterised case below builds three Targets, one per state,
    # so the assertion is performed inside the parametrise harness.
    assert target.state in {"enabled", "disabled", "dryrun"}


def _check_timeout(_, target):
    assert target._timeout == 600


def _check_exclusive(_, target):
    assert target.mode is ExecutionMode.SERIAL


def _check_custom_classes(_, target):
    assert target.TargetLock is target._expected_lock_class
    assert target.Connection is target._expected_conn_class


def _build_state_target(mock_config):
    """Build three Targets, one per supported state; return last for shape checks."""
    last = None
    for state in ("enabled", "disabled", "dryrun"):
        t = Target(mock_config, "host.example.com", state=state)  # type: ignore[arg-type]
        assert t.state == state
        last = t
    return last


def _build_custom_classes_target(mock_config):
    """Build a Target with mocked lock/connection classes; stash them for the check."""
    lock_class = MagicMock()
    conn_class = MagicMock()
    t = Target(  # type: ignore[arg-type]
        mock_config,
        "host.example.com",
        lock=lock_class,  # ty: ignore[invalid-argument-type]
        connection=conn_class,  # ty: ignore[invalid-argument-type]
    )
    # Smuggle the expected classes into the instance so the shared check fn
    # can compare without leaking the parametrise builder's locals.
    t._expected_lock_class = lock_class  # ty: ignore[unresolved-attribute]
    t._expected_conn_class = conn_class  # ty: ignore[unresolved-attribute]
    return t


@pytest.mark.parametrize(
    ("builder", "check"),
    [
        pytest.param(
            lambda cfg: Target(cfg, "test-host.example.com"),  # type: ignore[arg-type]
            _check_defaults,
            id="defaults",
        ),
        pytest.param(
            lambda cfg: Target(cfg, "test-host.example.com:2222"),  # type: ignore[arg-type]
            _check_port,
            id="with_port",
        ),
        pytest.param(
            lambda cfg: Target(  # type: ignore[arg-type]
                cfg, "host.example.com", {"standard": {"bash": "5.1-1.2"}}
            ),
            _check_packages,
            id="with_packages",
        ),
        pytest.param(_build_state_target, _check_state, id="with_state"),
        pytest.param(
            lambda cfg: Target(cfg, "host.example.com", timeout=600),  # type: ignore[arg-type]
            _check_timeout,
            id="with_timeout",
        ),
        pytest.param(
            lambda cfg: Target(  # type: ignore[arg-type]
                cfg, "host.example.com", mode=ExecutionMode.SERIAL
            ),
            _check_exclusive,
            id="with_exclusive",
        ),
        pytest.param(
            _build_custom_classes_target, _check_custom_classes, id="custom_classes"
        ),
    ],
)
def test_target_init(mock_config, builder, check):
    """Parametrised Target init coverage (was seven separate test_target_init_* functions)."""
    target = builder(mock_config)
    check(mock_config, target)


# --- String representation ---


def test_target_repr(mock_config):
    """Test Target __repr__."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    assert "Target" in repr(target)
    assert "host.example.com" in repr(target)


def test_target_str(mock_config):
    """Test Target __str__ returns hostname."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    assert str(target) == "host.example.com"


# --- last* methods ---


def test_last_methods_empty(mock_config):
    """Test last* methods return empty strings when no output."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    assert target.lastin() == ""
    assert target.lastout() == ""
    assert target.lasterr() == ""
    assert target.lastexit() == ""


def test_last_methods_with_output(mock_config):
    """Test last* methods after appending output."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.out = HostLog()
    target.out.append(["ls -la", "file1\nfile2\n", "warning\n", 0, 5])

    assert target.lastin() == "ls -la"
    assert "file1" in target.lastout()
    assert "warning" in target.lasterr()
    assert target.lastexit() == 0


# --- lock/unlock ---


def test_target_lock_delegates(mock_config):
    """Test lock() delegates to _lock."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target._lock = MagicMock()

    target.lock("test comment")
    target._lock.lock.assert_called_once_with("test comment")


def test_target_unlock_delegates(mock_config):
    """Test unlock() delegates to _lock."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target._lock = MagicMock()

    target.unlock()
    target._lock.unlock.assert_called_once_with(False)


def test_target_unlock_with_force(mock_config):
    """Test unlock(force=True) passes force flag."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target._lock = MagicMock()

    target.unlock(force=True)
    target._lock.unlock.assert_called_once_with(True)


# --- run() state machine ---


def test_run_enabled_executes_command(mock_config):
    """Test run() in enabled state executes the command."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
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
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.state = "dryrun"

    target.run("rm -rf /")

    target.connection.run.assert_not_called()
    assert "dryrun" in target.lastout()


def test_run_disabled_does_not_execute(mock_config):
    """Test run() in disabled state does not execute command."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.state = "disabled"

    target.run("some command")

    target.connection.run.assert_not_called()


def test_run_handles_command_timeout(mock_config):
    """Test run() catches CommandTimeoutError and sets exit code to -1."""
    from mtui.hosts.connection import CommandTimeoutError

    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.connection.run.side_effect = CommandTimeoutError("echo hello")
    target.state = "enabled"

    target.run("echo hello")

    # Should not raise; exit code should be -1
    assert target.lastexit() == -1


def test_run_handles_generic_exception(mock_config):
    """Test run() catches generic exceptions and sets exit code to -1."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.connection.run.side_effect = OSError("connection lost")
    target.state = "enabled"

    target.run("echo hello")

    assert target.lastexit() == -1


# --- reboot ---


def test_target_reboot_dispatches_fire_and_forget(mock_config):
    """reboot() sends the command fire-and-forget via the connection."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()

    target.reboot("systemctl reboot")
    target.connection.fire_and_forget.assert_called_once_with("systemctl reboot")


# --- reconnect ---


def test_reconnect_delegates(mock_config):
    """Test reconnect() delegates to connection, passing backoff by keyword.

    ``Connection.reconnect``'s second positional parameter is ``timeout``,
    so ``backoff`` must be passed by keyword or it is silently dropped.
    """
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()

    target.reconnect(3, True)

    target.connection.reconnect.assert_called_once_with(3, backoff=True)


# --- set_timeout ---


def test_set_timeout(mock_config):
    """Test set_timeout updates both connection and internal timeout."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()

    target.set_timeout(600)

    assert target._timeout == 600
    assert target.connection.timeout == 600


# --- close ---


def test_close_unlocks_and_closes(mock_config):
    """Test close() unlocks and closes the connection."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.connection.is_active.return_value = True
    target._lock = MagicMock()

    target.close()

    target._lock.unlock.assert_called_once_with(False)
    target.connection.close.assert_called_once()


def test_close_with_reboot(mock_config):
    """Test close() with reboot action."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
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
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.connection.is_active.side_effect = Exception("connection lost")

    target.close()  # should not raise

    target.connection.close.assert_called_once()


# --- _parse_packages ---


def test_parse_packages_standard(mock_config):
    """Test _parse_packages with 'standard' key."""
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
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
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
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
    target = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    target.system = MagicMock()
    target.system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    target._pkgs = None

    result = target._parse_packages()
    assert result == {}


# NOTE: report_* coverage moved to tests/test_reporter.py


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


# --- __eq__ / __hash__ contract (regression for hostname/system mismatch) ---


def test_target_eq_same_hostname_different_system(mock_config):
    """Targets with the same hostname must be equal regardless of system.

    Regression: previously __eq__ compared self.system while __hash__
    used hostname, breaking the data-model contract.
    """
    t1 = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    t2 = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    t1.system = MagicMock()
    t2.system = MagicMock()  # different system instance

    assert t1 == t2
    assert hash(t1) == hash(t2)


def test_target_eq_different_hostname(mock_config):
    """Targets with different hostnames must be unequal."""
    t1 = Target(mock_config, "host1.example.com")  # type: ignore[arg-type]
    t2 = Target(mock_config, "host2.example.com")  # type: ignore[arg-type]

    assert t1 != t2
    assert hash(t1) != hash(t2)


def test_target_eq_non_target_returns_notimplemented(mock_config):
    """Comparing Target to a non-Target must defer to the other side."""
    t = Target(mock_config, "host.example.com")  # type: ignore[arg-type]

    # __eq__ should return NotImplemented; the public `==` falls back to
    # identity comparison and yields False.
    assert t.__eq__("host.example.com") is NotImplemented
    assert (t == "host.example.com") is False


def test_target_set_dedup_by_hostname(mock_config):
    """Two Target objects with the same hostname collapse in a set."""
    t1 = Target(mock_config, "host.example.com")  # type: ignore[arg-type]
    t2 = Target(mock_config, "host.example.com")  # type: ignore[arg-type]

    assert len({t1, t2}) == 1


# ---------------------------------------------------------------------------
# connect  # noqa: ERA001
# ---------------------------------------------------------------------------


def test_connect_success_wires_up_lock_and_system(mock_config, monkeypatch):
    """Successful connect builds a connection, lock, parses system + packages."""
    conn_class = MagicMock()
    lock_class = MagicMock()
    lock_class.return_value.is_locked.return_value = False
    fake_system = MagicMock()
    fake_system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    monkeypatch.setattr(
        "mtui.hosts.target.target.parse_system", lambda _conn: (fake_system, False)
    )
    target = Target(  # type: ignore[arg-type]
        mock_config,
        "host.example.com",
        connection=conn_class,  # ty: ignore[invalid-argument-type]
        lock=lock_class,  # ty: ignore[invalid-argument-type]
    )
    target.connect()
    conn_class.assert_called_once()
    assert target.connection is conn_class.return_value
    assert target.system is fake_system
    assert target.transactional is False


def test_connect_warns_when_already_locked(mock_config, monkeypatch, caplog):
    """A fresh pre-existing lock is logged at warning but does not abort connect."""
    conn_class = MagicMock()
    lock_class = MagicMock()
    lock_class.return_value.is_locked.return_value = True
    lock_class.return_value.reap_if_stale.return_value = False
    lock_class.return_value.locked_by_msg.return_value = "locked by alice"
    fake_system = MagicMock()
    fake_system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    monkeypatch.setattr(
        "mtui.hosts.target.target.parse_system", lambda _conn: (fake_system, False)
    )
    target = Target(  # type: ignore[arg-type]
        mock_config,
        "host.example.com",
        connection=conn_class,  # ty: ignore[invalid-argument-type]
        lock=lock_class,  # ty: ignore[invalid-argument-type]
    )
    with caplog.at_level("WARNING", logger="mtui.target"):
        target.connect()
    assert any("locked by alice" in r.message for r in caplog.records)


def test_connect_reaps_stale_lock_without_warning(mock_config, monkeypatch, caplog):
    """A stale lock is reaped on connect; no 'locked by' warning is emitted."""
    conn_class = MagicMock()
    lock_class = MagicMock()
    lock_class.return_value.is_locked.return_value = True
    lock_class.return_value.reap_if_stale.return_value = True
    fake_system = MagicMock()
    fake_system.get_base.return_value = Product("SLES", "15-SP5", "x86_64")
    monkeypatch.setattr(
        "mtui.hosts.target.target.parse_system", lambda _conn: (fake_system, False)
    )
    target = Target(  # type: ignore[arg-type]
        mock_config,
        "host.example.com",
        connection=conn_class,  # ty: ignore[invalid-argument-type]
        lock=lock_class,  # ty: ignore[invalid-argument-type]
    )
    with caplog.at_level("WARNING", logger="mtui.target"):
        target.connect()
    lock_class.return_value.reap_if_stale.assert_called_once()
    lock_class.return_value.locked_by_msg.assert_not_called()
    assert not any("locked by" in r.message for r in caplog.records)


def test_connect_failure_logs_critical_and_reraises(mock_config, caplog):
    """A connection-time exception is logged at critical and propagated."""
    conn_class = MagicMock(side_effect=OSError("network down"))
    target = Target(  # type: ignore[arg-type]
        mock_config,
        "host.example.com",
        connection=conn_class,  # ty: ignore[invalid-argument-type]
    )
    with (
        caplog.at_level("CRITICAL", logger="mtui.target"),
        pytest.raises(OSError, match="network down"),
    ):
        target.connect()
    assert any("host.example.com" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# reload_system / set_repo
# ---------------------------------------------------------------------------


def test_reload_system_replaces_system_and_transactional(mock_config, monkeypatch):
    target = Target(mock_config, "h.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    new_system = MagicMock()
    monkeypatch.setattr(
        "mtui.hosts.target.target.parse_system", lambda _conn: (new_system, True)
    )
    target.reload_system()
    assert target.system is new_system
    assert target.transactional is True


# NOTE: set_repo coverage moved to tests/test_repo_manager.py


# ---------------------------------------------------------------------------
# query_versions / query_package_versions
# ---------------------------------------------------------------------------


def test_query_versions_enabled_updates_packages(mock_target, monkeypatch):
    """``enabled`` calls ``query_package_versions`` and updates ``Package.current``."""
    mock_target.state = "enabled"
    new_versions = {"bash": RPMVersion("5.2-1"), "openssl": RPMVersion("3.0.9-1")}
    monkeypatch.setattr(
        Target, "query_package_versions", lambda self, pkgs: new_versions
    )
    mock_target.query_versions()
    assert mock_target.packages["bash"].current == RPMVersion("5.2-1")
    assert mock_target.packages["openssl"].current == RPMVersion("3.0.9-1")


def test_query_versions_dryrun_logs_and_appends(mock_config):
    target = Target(mock_config, "h.example.com")  # type: ignore[arg-type]
    target.state = "dryrun"
    target.packages = {"bash": Package("bash")}
    target.query_versions()
    assert target.lastout() == "dryrun\n"


def test_query_versions_disabled_appends_empty(mock_config):
    target = Target(mock_config, "h.example.com")  # type: ignore[arg-type]
    target.state = "disabled"
    target.packages = {"bash": Package("bash")}
    target.query_versions()
    assert target.lastout() == ""
    assert target.lastexit() == 0


def test_query_package_versions_rpm_path(mock_target):
    """Non-ubuntu systems use ``rpm -q``."""
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = "bash 5.1-1\nopenssl 3.0-1\n"
    mock_target.connection.stderr = ""
    out = mock_target.query_package_versions(["bash", "openssl"])
    cmd = mock_target.connection.run.call_args[0][0]
    assert cmd.startswith("rpm -q")
    assert out["bash"] == RPMVersion("5.1-1")
    assert out["openssl"] == RPMVersion("3.0-1")


def test_query_package_versions_ubuntu_path(mock_target):
    """Ubuntu systems use ``dpkg-query``."""
    mock_target.system = MagicMock()
    mock_target.system.get_base.return_value = Product("ubuntu", "22.04", "x86_64")
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = "bash 5.1-1\n"
    mock_target.connection.stderr = ""
    mock_target.query_package_versions(["bash"])
    cmd = mock_target.connection.run.call_args[0][0]
    assert cmd.startswith("dpkg-query")


def test_query_package_versions_not_installed_returns_none(mock_target):
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = "package missing-pkg is not installed\n"
    mock_target.connection.stderr = ""
    out = mock_target.query_package_versions(["missing-pkg"])
    assert out == {"missing-pkg": None}


def test_query_package_versions_keeps_max_on_duplicate(mock_target):
    """Duplicate package lines collapse to the highest version."""
    mock_target.state = "enabled"
    mock_target.connection.run.return_value = 0
    mock_target.connection.stdout = "bash 5.0-1\nbash 5.2-1\nbash 5.1-1\n"
    mock_target.connection.stderr = ""
    out = mock_target.query_package_versions(["bash"])
    assert out["bash"] == RPMVersion("5.2-1")


# NOTE: run_zypper coverage moved to tests/test_repo_manager.py


# ---------------------------------------------------------------------------
# run() additional branches
# ---------------------------------------------------------------------------


def test_run_assertion_error_swallowed_with_debug_log(mock_config, caplog):
    """A zombie ``AssertionError`` from the connection is swallowed at debug level."""
    target = Target(mock_config, "h.example.com")  # type: ignore[arg-type]
    target.connection = MagicMock()
    target.connection.run.side_effect = AssertionError("zombie")
    target.state = "enabled"
    with caplog.at_level("DEBUG", logger="mtui.target"):
        target.run("noop")
    assert target.out == []  # nothing appended on this branch
    assert any("zombie" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# shell  # noqa: ERA001
# ---------------------------------------------------------------------------


def test_shell_delegates_to_connection(mock_target):
    mock_target.shell()
    mock_target.connection.shell.assert_called_once()


def test_shell_logs_on_failure(mock_target, caplog):
    mock_target.connection.shell.side_effect = RuntimeError("no tty")
    with caplog.at_level("ERROR", logger="mtui.target"):
        mock_target.shell()
    assert any("failed to spawn shell" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# sftp_put / sftp_get
# ---------------------------------------------------------------------------


def test_sftp_put_oserror_logged(mock_target, caplog):
    mock_target.state = "enabled"
    mock_target.connection.sftp_put.side_effect = OSError("disk full")
    with caplog.at_level("ERROR", logger="mtui.target"):
        mock_target.sftp_put(Path("/local"), Path("/remote"))
    assert any("failed to send" in r.message for r in caplog.records)


def test_sftp_get_file_enabled(mock_target):
    mock_target.state = "enabled"
    mock_target.sftp_get(Path("/remote/file"), Path("/local/file"))
    mock_target.connection.sftp_get.assert_called_once()
    mock_target.connection.sftp_get_folder.assert_not_called()


def test_sftp_get_folder_enabled(mock_target):
    """Trailing-slash remote uses ``sftp_get_folder``.

    NB: the parameter is typed ``Path`` but ``Path('/x/') == Path('/x')`` so
    the trailing-slash branch is only reachable when callers pass a raw
    string (or a string-coerced ``Path``). This locks in that behaviour.
    """
    mock_target.state = "enabled"
    mock_target.sftp_get("/remote/dir/", Path("/local"))
    mock_target.connection.sftp_get_folder.assert_called_once()
    mock_target.connection.sftp_get.assert_not_called()


def test_sftp_get_dryrun_does_nothing(mock_target):
    mock_target.state = "dryrun"
    mock_target.sftp_get(Path("/remote/file"), Path("/local"))
    mock_target.connection.sftp_get.assert_not_called()
    mock_target.connection.sftp_get_folder.assert_not_called()


def test_sftp_get_oserror_logged(mock_target, caplog):
    mock_target.state = "enabled"
    mock_target.connection.sftp_get.side_effect = OSError("perm denied")
    with caplog.at_level("ERROR", logger="mtui.target"):
        mock_target.sftp_get(Path("/remote/file"), Path("/local"))
    assert any("failed to get" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# is_locked / unlock
# ---------------------------------------------------------------------------


def test_is_locked_delegates(mock_target):
    mock_target._lock.is_locked.return_value = True
    assert mock_target.is_locked() is True


def test_unlock_target_locked_reraises(mock_target, caplog):
    mock_target._lock.unlock.side_effect = TargetLockedError(
        {"user": "bob", "timestamp": "now", "comment": ""}
    )
    with (
        caplog.at_level("WARNING", logger="mtui.target"),
        pytest.raises(TargetLockedError),
    ):
        mock_target.unlock()


# ---------------------------------------------------------------------------
# add_history
# ---------------------------------------------------------------------------


def test_add_history_writes_entry(mock_target):
    mock_target.state = "enabled"
    fh = MagicMock()
    mock_target.connection.sftp_open.return_value = fh
    mock_target.add_history(["update", "msg"])
    fh.write.assert_called_once()
    fh.close.assert_called_once()


def test_add_history_logs_when_open_fails(mock_target, caplog):
    mock_target.state = "enabled"
    mock_target.connection.sftp_open.side_effect = OSError("perm")
    with caplog.at_level("ERROR", logger="mtui.target"):
        mock_target.add_history(["update", "msg"])
    assert any("failed to open history file" in r.message for r in caplog.records)


def test_add_history_swallows_write_failure(mock_target):
    """Write/close failures are silently swallowed (current behaviour)."""
    mock_target.state = "enabled"
    fh = MagicMock()
    fh.write.side_effect = OSError("write failed")
    mock_target.connection.sftp_open.return_value = fh
    # Must not raise.
    mock_target.add_history(["update", "msg"])


# ---------------------------------------------------------------------------
# sftp_listdir / sftp_remove
# ---------------------------------------------------------------------------


def test_sftp_listdir_returns_connection_result(mock_target):
    mock_target.connection.sftp_listdir.return_value = ["a", "b"]
    assert mock_target.sftp_listdir(Path("/")) == ["a", "b"]


def test_sftp_listdir_enoent_returns_empty(mock_target, caplog):
    err = OSError(errno.ENOENT, "missing")
    mock_target.connection.sftp_listdir.side_effect = err
    with caplog.at_level("DEBUG", logger="mtui.target"):
        assert mock_target.sftp_listdir(Path("/nope")) == []


def test_sftp_remove_file_success(mock_target):
    mock_target.sftp_remove(Path("/some/file"))
    mock_target.connection.sftp_remove.assert_called_once()
    mock_target.connection.sftp_rmdir.assert_not_called()


def test_sftp_remove_enoent_logged_silently(mock_target, caplog):
    mock_target.connection.sftp_remove.side_effect = OSError(errno.ENOENT, "missing")
    with caplog.at_level("DEBUG", logger="mtui.target"):
        mock_target.sftp_remove(Path("/missing"))
    mock_target.connection.sftp_rmdir.assert_not_called()


def test_sftp_remove_other_oserror_falls_back_to_rmdir(mock_target):
    """Non-ENOENT OSError on remove is treated as 'might be a directory'."""
    mock_target.connection.sftp_remove.side_effect = OSError(errno.EISDIR, "is dir")
    mock_target.sftp_remove(Path("/some/dir"))
    mock_target.connection.sftp_rmdir.assert_called_once()


def test_sftp_remove_rmdir_failure_warns(mock_target, caplog):
    mock_target.connection.sftp_remove.side_effect = OSError(errno.EISDIR, "is dir")
    mock_target.connection.sftp_rmdir.side_effect = OSError("nonempty")
    with caplog.at_level("WARNING", logger="mtui.target"):
        mock_target.sftp_remove(Path("/some/dir"))
    assert any("unable to remove" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# close additional branches
# ---------------------------------------------------------------------------


def test_close_poweroff(mock_target):
    mock_target.connection.is_active.return_value = True
    mock_target.state = "enabled"
    mock_target.close(action="poweroff")
    halt_calls = [
        c for c in mock_target.connection.run.call_args_list if "halt" in str(c)
    ]
    assert halt_calls
    mock_target.connection.close.assert_called_once()


# NOTE: report_* coverage moved to tests/test_reporter.py
# NOTE: doer/check coverage moved to tests/test_doers.py
