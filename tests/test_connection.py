from io import StringIO
from unittest.mock import MagicMock, patch

import paramiko
import pytest

from mtui.connection import CommandTimeoutError, Connection, policy_from_config


@pytest.fixture
def mock_ssh_client(monkeypatch):
    """Fixture to mock paramiko.SSHClient within the connection module."""
    mock_client = MagicMock()
    # The Connection class has `from paramiko import SSHClient`, so we patch it there
    monkeypatch.setattr("mtui.connection.SSHClient", lambda: mock_client)

    # The is_active() method accesses _transport directly.
    mock_transport = MagicMock()
    mock_transport.is_active.return_value = True
    mock_client.get_transport.return_value = mock_transport  # for new_session
    mock_client._transport = mock_transport  # for is_active

    return mock_client


@pytest.fixture
def mock_ssh_config(monkeypatch):
    """Fixture to mock paramiko.SSHConfig within the connection module."""
    mock_config = MagicMock()
    monkeypatch.setattr("mtui.connection.SSHConfig", lambda: mock_config)
    mock_config.lookup.return_value = {}
    return mock_config


@pytest.fixture
def mock_path(monkeypatch):
    """Fixture to mock pathlib.Path."""
    mock_path_instance = MagicMock()
    string_io = StringIO("")  # An empty file for config parsing
    mock_path_instance.expanduser.return_value.open.return_value.__enter__.return_value = string_io
    monkeypatch.setattr("mtui.connection.Path", lambda path: mock_path_instance)
    return mock_path_instance


def test_connection_init_success(mock_ssh_client, mock_ssh_config, mock_path):
    """Test successful Connection initialization."""
    conn = Connection("test_host", 22, 300)

    mock_ssh_client.load_system_host_keys.assert_called_once()
    mock_ssh_client.set_missing_host_key_policy.assert_called_once()
    mock_ssh_client.connect.assert_called_once_with(
        hostname="test_host", port=22, username="root", key_filename=None, sock=None
    )
    assert conn.hostname == "test_host"
    assert conn.port == 22
    assert conn.timeout == 300


def test_connection_init_auth_fallback(mock_ssh_client, mock_ssh_config, mock_path):
    """Test password fallback authentication."""
    mock_ssh_client.connect.side_effect = [
        paramiko.AuthenticationException,
        None,  # second call succeeds
    ]
    with patch("getpass.getpass", return_value="password") as mock_getpass:
        Connection("test_host", 22, 300)

        assert mock_getpass.call_count == 1
        assert mock_ssh_client.connect.call_count == 2
        mock_ssh_client.connect.assert_any_call(
            hostname="test_host", port=22, username="root", key_filename=None, sock=None
        )
        mock_ssh_client.connect.assert_any_call(
            hostname="test_host",
            port=22,
            username="root",
            password="password",
            sock=None,
        )


def test_run_command_success(mock_ssh_client, mock_ssh_config, mock_path):
    """Test successful command execution with the 'run' method."""
    conn = Connection("test_host", 22, 300)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    mock_session.recv_exit_status.return_value = 0
    # Make recv/recv_stderr blocking by returning data only once
    mock_session.recv.side_effect = [b"output", b""]
    mock_session.recv_stderr.side_effect = [b"error", b""]
    # And make the ready checks reflect that
    mock_session.recv_ready.side_effect = [True, False]
    mock_session.recv_stderr_ready.side_effect = [True, False]

    with patch("select.select", return_value=([mock_session], [], [])):
        exit_code = conn.run("ls -l")

    assert exit_code == 0
    assert conn.stdout == "output"
    assert conn.stderr == "error"
    mock_session.exec_command.assert_called_once_with("ls -l")


def test_run_command_timeout(mock_ssh_client, mock_ssh_config, mock_path):
    """Test command timeout in the 'run' method."""
    conn = Connection("test_host", 22, 300)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    with (
        patch("select.select", return_value=([], [], [])),
        patch("builtins.input", return_value="n"),
        pytest.raises(CommandTimeoutError),
    ):
        conn.run("sleep 10")


def test_connection_invalid_port_warns_and_falls_back(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Invalid SSH port string should log a warning and fall back to 22."""
    import logging

    with caplog.at_level(logging.WARNING, logger="mtui.connection"):
        conn = Connection("test_host", "not-a-number", 300)

    assert conn.port == 22
    assert any(
        "invalid SSH port" in rec.message and "not-a-number" in rec.message
        for rec in caplog.records
    )


# --- ssh_strict_host_key_checking host-key policy mapping ---


@pytest.mark.parametrize(
    ("name", "expected_cls"),
    [
        ("auto_add", paramiko.AutoAddPolicy),
        ("warn", paramiko.WarningPolicy),
        ("reject", paramiko.RejectPolicy),
    ],
)
def test_policy_from_config_known(name, expected_cls):
    """Known config values map to the matching paramiko policy class."""
    policy = policy_from_config(name)
    assert isinstance(policy, expected_cls)


def test_policy_from_config_unknown_falls_back_with_warning(caplog):
    """Unknown config values warn and fall back to AutoAddPolicy."""
    import logging

    with caplog.at_level(logging.WARNING, logger="mtui.connection"):
        policy = policy_from_config("garbage")

    assert isinstance(policy, paramiko.AutoAddPolicy)
    assert any(
        "unknown ssh_strict_host_key_checking" in rec.message
        and "garbage" in rec.message
        for rec in caplog.records
    )


def test_connection_default_policy_is_auto_add(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """No explicit policy passed -> Connection still uses AutoAddPolicy."""
    Connection("test_host", 22, 300)

    args, _ = mock_ssh_client.set_missing_host_key_policy.call_args
    assert isinstance(args[0], paramiko.AutoAddPolicy)


def test_connection_uses_provided_policy(mock_ssh_client, mock_ssh_config, mock_path):
    """Explicit policy is forwarded to the paramiko client."""
    policy = paramiko.RejectPolicy()
    Connection("test_host", 22, 300, missing_host_key_policy=policy)

    args, _ = mock_ssh_client.set_missing_host_key_policy.call_args
    assert args[0] is policy


# ---------------------------------------------------------------------------
# misc helpers
# ---------------------------------------------------------------------------


def test_command_timeout_str_formats_command():
    """``CommandTimeoutError.__str__`` returns ``repr(self.command)``."""
    assert str(CommandTimeoutError("ls -l")) == "'ls -l'"


def test_connection_repr(mock_ssh_client, mock_ssh_config, mock_path):
    conn = Connection("h", 22, 300)
    text = repr(conn)
    assert "Connection" in text
    assert "h" in text
    assert "22" in text


# ---------------------------------------------------------------------------
# connect() — config error and additional auth/exception branches
# ---------------------------------------------------------------------------


def test_connect_logs_non_enoent_ssh_config_error(
    mock_ssh_client, mock_ssh_config, monkeypatch, caplog
):
    """An OSError other than ENOENT when reading ~/.ssh/config is logged."""
    import errno as _errno

    fake_path = MagicMock()
    fake_path.expanduser.return_value.open.side_effect = OSError(
        _errno.EACCES, "denied"
    )
    monkeypatch.setattr("mtui.connection.Path", lambda _path: fake_path)
    with caplog.at_level("WARNING", logger="mtui.connection"):
        Connection("test_host", 22, 300)
    assert any("denied" in r.message for r in caplog.records)


def test_connect_wrong_password_reraises(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Failing both key and password auth re-raises the second exception."""
    mock_ssh_client.connect.side_effect = [
        paramiko.AuthenticationException,
        paramiko.AuthenticationException,
    ]
    with (
        patch("getpass.getpass", return_value="wrong"),
        pytest.raises(paramiko.AuthenticationException),
        caplog.at_level("ERROR", logger="mtui.connection"),
    ):
        Connection("test_host", 22, 300)


def test_connect_sshexception_propagates(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Generic SSHException is logged and re-raised."""
    mock_ssh_client.connect.side_effect = paramiko.SSHException("boom")
    with (
        caplog.at_level("ERROR", logger="mtui.connection"),
        pytest.raises(paramiko.SSHException),
    ):
        Connection("test_host", 22, 300)


def test_connect_oserror_is_logged_not_raised(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Network-level OSError on connect is logged but not re-raised."""
    mock_ssh_client.connect.side_effect = OSError("network")
    with caplog.at_level("ERROR", logger="mtui.connection"):
        Connection("test_host", 22, 300)
    assert any("No valid connection" in r.message for r in caplog.records)


def test_connect_unknown_exception_propagates(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Truly unexpected exceptions are logged at debug and re-raised."""
    mock_ssh_client.connect.side_effect = RuntimeError("weird")
    with (
        caplog.at_level("DEBUG", logger="mtui.connection"),
        pytest.raises(RuntimeError, match="weird"),
    ):
        Connection("test_host", 22, 300)


# ---------------------------------------------------------------------------
# reconnect  # noqa: ERA001
# ---------------------------------------------------------------------------


def test_reconnect_no_op_when_already_active(
    mock_ssh_client, mock_ssh_config, mock_path
):
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = True
    # Reset the connect counter so we observe just the reconnect.
    mock_ssh_client.connect.reset_mock()
    conn.reconnect(retry=3)
    mock_ssh_client.connect.assert_not_called()


def test_reconnect_raises_after_exhausting_retries(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """If reconnects don't restore activity, ``ConnectionError`` is raised."""
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = False
    monkeypatch.setattr(
        "mtui.connection.select.select", lambda *_a, **_kw: ([], [], [])
    )
    with pytest.raises(ConnectionError, match="Failed to reconnect"):
        conn.reconnect(retry=2, timeout=0)


def test_reconnect_backoff_grows_timeout(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """The backoff branch widens the select timeout per attempt."""
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = False
    sel_mock = MagicMock(return_value=([], [], []))
    monkeypatch.setattr("mtui.connection.select.select", sel_mock)
    with pytest.raises(ConnectionError):
        conn.reconnect(retry=2, timeout=1, backoff=True)
    # First attempt uses raw timeout, then 2*(1 + 5*1) = 12, then 2*(1 + 5*2) = 22.
    timeouts = [c.args[3] for c in sel_mock.call_args_list]
    assert timeouts[0] == 1
    assert timeouts[1] == 12


# ---------------------------------------------------------------------------
# new_session / close_session / __run_command branches
# ---------------------------------------------------------------------------


def test_new_session_returns_none_when_no_transport(
    mock_ssh_client, mock_ssh_config, mock_path
):
    conn = Connection("h", 22, 300)
    mock_ssh_client.get_transport.return_value = None
    assert conn.new_session() is None


def test_new_session_swallows_logger_attach_failure(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """Failing to attach the NullHandler logs at debug but doesn't crash."""
    conn = Connection("h", 22, 300)
    transport = mock_ssh_client.get_transport.return_value
    transport.get_log_channel.side_effect = RuntimeError("no log")
    # ``open_session`` still works.
    transport.open_session.return_value = MagicMock()
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        sess = conn.new_session()
    assert sess is transport.open_session.return_value


def test_new_session_logs_when_open_session_fails(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    conn = Connection("h", 22, 300)
    transport = mock_ssh_client.get_transport.return_value
    transport.open_session.side_effect = paramiko.SSHException("kaboom")
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        assert conn.new_session() is None
    assert any("failed to open new session" in r.message for r in caplog.records)


def test_close_session_swallows_errors(caplog):
    sess = MagicMock()
    sess.shutdown.side_effect = OSError("already closed")
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        Connection.close_session(sess)
    assert any("ignoring error" in r.message for r in caplog.records)


def test_close_session_none_is_noop():
    Connection.close_session(None)  # must not raise


def test_run_command_retries_then_raises_reconnect_failed(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """If ``__run_command`` keeps failing, ``ReConnectFailed`` is raised."""
    from mtui.messages import ReConnectFailed

    conn = Connection("h", 22, 300)
    # Force ``new_session`` to always return None so __run_command yields None.
    mock_ssh_client.get_transport.return_value = None
    # ``Connection`` uses ``__slots__``; patch on the class instead of the instance.
    monkeypatch.setattr(Connection, "reconnect", lambda *_args, **_kw: None)
    with pytest.raises(ReConnectFailed):
        conn.run("cmd")


# ---------------------------------------------------------------------------
# run() — recv TimeoutError branch
# ---------------------------------------------------------------------------


def test_run_recv_timeout_is_swallowed(mock_ssh_client, mock_ssh_config, mock_path):
    """A TimeoutError during recv is caught; the loop sleeps and continues."""
    conn = Connection("h", 22, 300)
    sess = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = sess
    sess.recv_exit_status.return_value = 0
    # First iteration: recv_ready True but recv raises TimeoutError; second: clean.
    sess.recv_ready.side_effect = [True, True, False]
    sess.recv.side_effect = [TimeoutError("boom"), b"data", b""]
    sess.recv_stderr_ready.return_value = False
    with patch("mtui.connection.select.select", return_value=([sess], [], [])):
        rc = conn.run("cmd")
    assert rc == 0
    assert "data" in conn.stdout


# ---------------------------------------------------------------------------
# is_active / close
# ---------------------------------------------------------------------------


def test_is_active_no_transport_returns_false(
    mock_ssh_client, mock_ssh_config, mock_path
):
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport = None
    assert conn.is_active() is False


def test_is_active_delegates_to_transport(mock_ssh_client, mock_ssh_config, mock_path):
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = False
    assert conn.is_active() is False


def test_close_delegates_to_client(mock_ssh_client, mock_ssh_config, mock_path):
    conn = Connection("h", 22, 300)
    conn.close()
    mock_ssh_client.close.assert_called()


# ---------------------------------------------------------------------------
# SFTP family
# ---------------------------------------------------------------------------


@pytest.fixture
def sftp_client():
    """A bare SFTPClient mock."""
    return MagicMock()


@pytest.fixture
def conn_with_sftp(mock_ssh_client, mock_ssh_config, mock_path, sftp_client):
    """A ``Connection`` whose ``open_sftp`` returns the supplied SFTP mock."""
    mock_ssh_client.open_sftp.return_value = sftp_client
    return Connection("h", 22, 300)


def test_sftp_open_failure_returns_none(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    mock_ssh_client.open_sftp.side_effect = paramiko.SSHException("nope")
    conn = Connection("h", 22, 300)
    # Force is_active False so reconnect loop runs once and fails.
    mock_ssh_client._transport.is_active.return_value = False
    from mtui.messages import ReConnectFailed

    monkeypatch.setattr(Connection, "reconnect", lambda *_args, **_kw: None)
    with pytest.raises(ReConnectFailed):
        conn.sftp_listdir()


def test_sftp_put_creates_subdirs_and_chmods(conn_with_sftp, sftp_client):
    """``sftp_put`` walks the parent path, mkdirs each segment, then chmods."""
    from pathlib import Path as _Path

    conn_with_sftp.sftp_put(_Path("/local/file"), _Path("/srv/sub/file.txt"))
    # mkdir called for /, srv/, sub/  (the loop creates the parent path).
    assert sftp_client.mkdir.call_count >= 1
    sftp_client.put.assert_called_once_with("/local/file", "/srv/sub/file.txt")
    sftp_client.chmod.assert_called_once()
    sftp_client.close.assert_called()


def test_sftp_put_treats_existing_dir_as_success(conn_with_sftp, sftp_client, caplog):
    """An ``OSError`` on ``mkdir`` (e.g. EEXIST) is debug-logged and skipped."""
    from pathlib import Path as _Path

    sftp_client.mkdir.side_effect = OSError("File exists")
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        conn_with_sftp.sftp_put(_Path("/local"), _Path("/srv/x"))
    sftp_client.put.assert_called_once()


def test_sftp_get_calls_paramiko_get(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    conn_with_sftp.sftp_get(_Path("/remote"), _Path("/local"))
    sftp_client.get.assert_called_once_with("/remote", _Path("/local"))
    sftp_client.close.assert_called()


def test_sftp_get_folder_iterates_listdir(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    sftp_client.listdir.return_value = ["a", "b"]
    conn_with_sftp.sftp_get_folder(_Path("/remote"), _Path("/local/"))
    # 2 files → 2 ``get`` calls.
    assert sftp_client.get.call_count == 2


def test_sftp_listdir_returns_paramiko_listdir(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    sftp_client.listdir.return_value = ["a", "b"]
    assert conn_with_sftp.sftp_listdir(_Path("/x")) == ["a", "b"]
    sftp_client.close.assert_called()


def test_sftp_open_returns_handle(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    file_handle = MagicMock()
    sftp_client.open.return_value = file_handle
    assert conn_with_sftp.sftp_open(_Path("/x"), "r") is file_handle


def test_sftp_open_logs_and_reraises_on_failure(conn_with_sftp, sftp_client, caplog):
    """``sftp_open`` debug-logs the traceback and re-raises any error.

    NB: the source path also tries to ``sftp.close()`` when the local
    ``sftp`` happens to be a real ``paramiko.SFTPClient``. With a MagicMock
    that ``isinstance`` check is False, so ``close`` isn't called. This
    test just locks in the "log + re-raise" public contract.
    """
    from pathlib import Path as _Path

    sftp_client.open.side_effect = OSError("perm")
    with (
        caplog.at_level("DEBUG", logger="mtui.connection"),
        pytest.raises(OSError, match="perm"),
    ):
        conn_with_sftp.sftp_open(_Path("/x"), "r")
    assert any("Traceback" in r.message for r in caplog.records)


def test_sftp_remove_calls_paramiko_remove(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    conn_with_sftp.sftp_remove(_Path("/x"))
    sftp_client.remove.assert_called_once_with("/x")
    sftp_client.close.assert_called()


def test_sftp_remove_logs_oserror(conn_with_sftp, sftp_client, caplog):
    from pathlib import Path as _Path

    sftp_client.remove.side_effect = OSError("perm")
    with caplog.at_level("ERROR", logger="mtui.connection"):
        conn_with_sftp.sftp_remove(_Path("/x"))
    assert any("Can't remove" in r.message for r in caplog.records)


def test_sftp_rmdir_recursively_removes(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    sftp_client.listdir.return_value = ["a", "b"]
    conn_with_sftp.sftp_rmdir(_Path("/some/dir"))
    # Each child file is removed, then the directory itself.
    assert sftp_client.remove.call_count == 2
    sftp_client.rmdir.assert_called_once_with("/some/dir")


def test_sftp_readlink_returns_target(conn_with_sftp, sftp_client):
    from pathlib import Path as _Path

    sftp_client.readlink.return_value = "/real/path"
    assert conn_with_sftp.sftp_readlink(_Path("/link")) == "/real/path"
    sftp_client.close.assert_called()
