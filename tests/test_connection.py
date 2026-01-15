from io import StringIO
from unittest.mock import MagicMock, patch

import paramiko
import pytest

from mtui.connection import CommandTimeout, Connection


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
        conn = Connection("test_host", 22, 300)

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

    with patch("select.select", return_value=([], [], [])):
        with patch("builtins.input", return_value="n"):
            with pytest.raises(CommandTimeout):
                conn.run("sleep 10")
