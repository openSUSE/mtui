"""Deeper coverage for ``mtui.connection.Connection``.

Targets the remaining gaps from ``test_connection.py``:
* ``new_session`` NullHandler attach swallow
* ``__run_command`` exception path
* ``run`` "session lost" branch
* ``__invoke_shell`` retry path
* ``shell()`` happy path
* ``__sftp_open`` AttributeError
* ``sftp_open`` BaseException
* ``sftp_rmdir`` per-file remove OSError logging
"""

from __future__ import annotations

from io import StringIO
from pathlib import Path as _Path
from unittest.mock import MagicMock, patch

import paramiko
import pytest

from mtui.connection import Connection


@pytest.fixture
def mock_ssh_client(monkeypatch):
    """Patches ``mtui.connection.SSHClient`` to return a mock."""
    mock_client = MagicMock()
    monkeypatch.setattr("mtui.connection.SSHClient", lambda: mock_client)
    mock_transport = MagicMock()
    mock_transport.is_active.return_value = True
    mock_client.get_transport.return_value = mock_transport
    mock_client._transport = mock_transport
    return mock_client


@pytest.fixture
def mock_ssh_config(monkeypatch):
    """Patches ``mtui.connection.SSHConfig``."""
    mock_config = MagicMock()
    monkeypatch.setattr("mtui.connection.SSHConfig", lambda: mock_config)
    mock_config.lookup.return_value = {}
    return mock_config


@pytest.fixture
def mock_path(monkeypatch):
    """Patches ``mtui.connection.Path`` so the SSH config read becomes a no-op."""
    mock_path_instance = MagicMock()
    string_io = StringIO("")
    mock_path_instance.expanduser.return_value.open.return_value.__enter__.return_value = string_io
    monkeypatch.setattr("mtui.connection.Path", lambda _p: mock_path_instance)
    return mock_path_instance


# ---------------------------------------------------------------------------
# new_session: NullHandler attach swallow (line 292)
# ---------------------------------------------------------------------------


def test_new_session_swallows_get_log_channel_failure(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """``transport.get_log_channel()`` raising must not break ``new_session``."""
    conn = Connection("h", 22, 300)
    transport = mock_ssh_client.get_transport.return_value
    transport.get_log_channel.side_effect = RuntimeError("boom")
    transport.open_session.return_value = MagicMock()
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        sess = conn.new_session()
    assert sess is transport.open_session.return_value


# ---------------------------------------------------------------------------
# __run_command exception path closes session and returns None (349-352)
# ---------------------------------------------------------------------------


def test_run_command_exec_raises_closes_session_and_retries(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """``exec_command`` raising ``SSHException`` makes ``__run_command`` close
    the session and return None; ``run`` then retries via ``reconnect`` and
    eventually raises ``ReConnectFailed`` after ``RETRIES`` attempts.
    """
    from mtui.support.messages import ReConnectFailed

    conn = Connection("h", 22, 300)
    sess = MagicMock()
    sess.exec_command.side_effect = paramiko.SSHException("boom")
    mock_ssh_client.get_transport.return_value.open_session.return_value = sess

    # Patch reconnect (instance has __slots__ → patch on the class).
    monkeypatch.setattr(Connection, "reconnect", lambda *_a, **_kw: None)
    with pytest.raises(ReConnectFailed):
        conn.run("cmd")
    # The first attempt opened a session and we closed it.
    assert sess.exec_command.call_count >= 1


# ---------------------------------------------------------------------------
# run() — session lost mid-command (line 392)
# ---------------------------------------------------------------------------


def test_run_raises_session_lost_when_session_falsey(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """When ``select.select`` times out *and* the session is falsey, ``run``
    raises ``ConnectionError("Session lost during command execution on …")``.

    Exercises line 392 ``raise ConnectionError("Session lost ...")``.

    The production code reads:

        session = self.__run_command(command)   # must be truthy
        while not session:                      # exit immediately
            ...
        while True:
            if select.select([session], ...) == ([], [], []):
                if not session:                 # *now* must be falsey
                    raise ConnectionError("Session lost ...")

    So ``session`` has to flip from truthy to falsey across the two
    ``bool()`` checks. We use a counter on ``__bool__`` to do that.
    """

    class FlippingSession:
        """Stub session that is truthy initially, falsey after N bool checks.

        ``__run_command``'s walrus check counts; ``while not session`` and
        the timeout-branch ``if not session`` are the next two. We need
        the first two truthy and the third falsey.
        """

        def __init__(self) -> None:
            self.bool_calls = 0

        def __bool__(self) -> bool:
            self.bool_calls += 1
            return self.bool_calls < 3

        # paramiko Channel API the production code touches before the bool flip.
        def setblocking(self, _: int) -> None: ...
        def settimeout(self, _: int) -> None: ...
        def exec_command(self, _cmd: str) -> None: ...

    flipping = FlippingSession()
    mock_ssh_client.get_transport.return_value.open_session.return_value = flipping
    conn = Connection("h", 22, 300)
    with (
        patch("mtui.connection.select.select", return_value=([], [], [])),
        pytest.raises(ConnectionError, match="Session lost"),
    ):
        conn.run("cmd")


# ---------------------------------------------------------------------------
# __invoke_shell retry path (462-473) and shell() happy path (477-510)
# ---------------------------------------------------------------------------


def _stub_terminal(monkeypatch) -> MagicMock:
    """Patch the shell()-side TTY bits and return a fake stdin."""
    fake_stdin = MagicMock()
    fake_stdin.fileno.return_value = 0
    fake_stdin.read.return_value = ""  # empty → break

    monkeypatch.setattr("mtui.connection.sys.stdin", fake_stdin)
    monkeypatch.setattr(
        "mtui.connection.termios.tcgetattr", lambda *_a, **_kw: object()
    )
    monkeypatch.setattr("mtui.connection.termios.tcsetattr", lambda *_a, **_kw: None)
    monkeypatch.setattr("mtui.connection.tty.setraw", lambda *_a, **_kw: None)
    monkeypatch.setattr("mtui.connection.tty.setcbreak", lambda *_a, **_kw: None)
    monkeypatch.setattr("mtui.connection.termsize", lambda: (80, 24))
    return fake_stdin


def test_shell_invokes_shell_after_one_retry(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """First ``__invoke_shell`` returns None → ``shell()`` reconnects and
    retries; the second attempt succeeds and the recv loop completes.
    """
    conn = Connection("h", 22, 300)
    real_sess = MagicMock()
    real_sess.recv.return_value = b""  # empty → break out of select loop
    mock_ssh_client.get_transport.return_value.open_session.side_effect = [
        None,  # first __invoke_shell returns None
        real_sess,
    ]
    _stub_terminal(monkeypatch)
    monkeypatch.setattr(Connection, "reconnect", lambda *_a, **_kw: None)

    with patch(
        "mtui.connection.select.select",
        return_value=([real_sess], [], []),
    ):
        conn.shell()
    real_sess.invoke_shell.assert_called_once()


def test_shell_handles_stdin_branch(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """Cover the stdin-ready branch of ``shell()``."""
    conn = Connection("h", 22, 300)
    sess = MagicMock()
    sess.recv.return_value = b""
    mock_ssh_client.get_transport.return_value.open_session.return_value = sess
    fake_stdin = _stub_terminal(monkeypatch)

    with patch(
        "mtui.connection.select.select",
        side_effect=[
            ([fake_stdin], [], []),
            ([sess], [], []),
        ],
    ):
        conn.shell()


# ---------------------------------------------------------------------------
# __sftp_open returns None on AttributeError (line 523)
# ---------------------------------------------------------------------------


def test_sftp_open_attribute_error_returns_none(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """An ``AttributeError`` from ``open_sftp`` makes ``__sftp_open`` return
    None; ``__sftp_reconnect`` then retries (we exhaust retries → raise).
    """
    from mtui.support.messages import ReConnectFailed

    mock_ssh_client.open_sftp.side_effect = AttributeError("gone")
    conn = Connection("h", 22, 300)
    monkeypatch.setattr(Connection, "reconnect", lambda *_a, **_kw: None)
    with pytest.raises(ReConnectFailed):
        conn.sftp_listdir(_Path("/x"))


# ---------------------------------------------------------------------------
# sftp_open BaseException (line 714)
# ---------------------------------------------------------------------------


def test_sftp_open_baseexception_closes_and_reraises(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """A ``BaseException`` (here ``KeyboardInterrupt``) propagates after a
    debug log; the SFTP client is closed first via the local ``close()``.
    """
    real_sftp = MagicMock(spec=paramiko.SFTPClient)
    real_sftp.open.side_effect = KeyboardInterrupt
    mock_ssh_client.open_sftp.return_value = real_sftp
    conn = Connection("h", 22, 300)
    with pytest.raises(KeyboardInterrupt):
        conn.sftp_open(_Path("/x"), "r")
    real_sftp.close.assert_called_once()


# ---------------------------------------------------------------------------
# sftp_rmdir per-file remove OSError logged (750-751)
# ---------------------------------------------------------------------------


def test_sftp_rmdir_logs_per_file_remove_oserror(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """A per-file ``remove`` OSError is logged but does not abort the dir
    cleanup; the final ``rmdir`` still runs.
    """
    real_sftp = MagicMock()
    real_sftp.listdir.return_value = ["a", "b"]
    # First remove raises, second succeeds.
    real_sftp.remove.side_effect = [OSError("perm"), None]
    mock_ssh_client.open_sftp.return_value = real_sftp
    conn = Connection("h", 22, 300)
    with caplog.at_level("ERROR", logger="mtui.connection"):
        conn.sftp_rmdir(_Path("/d"))
    assert any("Can't remove" in r.message for r in caplog.records)
    real_sftp.rmdir.assert_called_once_with("/d")
