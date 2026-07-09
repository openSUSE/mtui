from io import StringIO
from unittest.mock import MagicMock, patch

import paramiko
import pytest

from mtui.hosts.connection import (
    CommandTimeoutError,
    Connection,
    policy_from_config,
)


@pytest.fixture
def mock_ssh_client(monkeypatch):
    """Fixture to mock paramiko.SSHClient within the connection module."""
    mock_client = MagicMock()
    # The Connection class has `from paramiko import SSHClient`, so we patch it there
    monkeypatch.setattr(
        "mtui.hosts.connection.connection.SSHClient", lambda: mock_client
    )

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
    monkeypatch.setattr(
        "mtui.hosts.connection.connection.SSHConfig", lambda: mock_config
    )
    mock_config.lookup.return_value = {}
    return mock_config


@pytest.fixture
def mock_path(monkeypatch):
    """Fixture to mock pathlib.Path."""
    mock_path_instance = MagicMock()
    string_io = StringIO("")  # An empty file for config parsing
    mock_path_instance.expanduser.return_value.open.return_value.__enter__.return_value = string_io
    monkeypatch.setattr(
        "mtui.hosts.connection.connection.Path", lambda path: mock_path_instance
    )
    return mock_path_instance


def test_connection_init_success(mock_ssh_client, mock_ssh_config, mock_path):
    """Test successful Connection initialization."""
    conn = Connection("test_host", 22, 300)

    mock_ssh_client.load_system_host_keys.assert_called_once()
    mock_ssh_client.set_missing_host_key_policy.assert_called_once()
    mock_ssh_client.connect.assert_called_once_with(
        hostname="test_host",
        port=22,
        username="root",
        key_filename=None,
        sock=None,
        timeout=300,
        banner_timeout=300,
        auth_timeout=300,
    )
    assert conn.hostname == "test_host"
    assert conn.port == 22
    assert conn.timeout == 300


def test_connection_key_auth_failure_reraises_without_password_prompt(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """A key-auth failure re-raises; there is no password fallback.

    MTUI requires working SSH key authentication. When key auth fails the
    ``AuthenticationException`` propagates to the caller (Target.connect,
    which reports ConnectingTargetFailedMessage) -- the connection never
    prompts for a password. This holds in both interactive and
    non-interactive sessions.
    """
    mock_ssh_client.connect.side_effect = paramiko.AuthenticationException

    with patch("getpass.getpass") as mock_getpass:
        with pytest.raises(paramiko.AuthenticationException):
            Connection("test_host", 22, 300)

        mock_getpass.assert_not_called()
        # Only the initial key-auth attempt happened; no password retry.
        assert mock_ssh_client.connect.call_count == 1


def test_connection_bad_host_key_reraises(mock_ssh_client, mock_ssh_config, mock_path):
    """A BadHostKeyException re-raises just like a plain auth failure."""
    mock_ssh_client.connect.side_effect = paramiko.BadHostKeyException(
        "test_host", MagicMock(), MagicMock()
    )

    with patch("getpass.getpass") as mock_getpass:
        with pytest.raises(paramiko.BadHostKeyException):
            Connection("test_host", 22, 300)

        mock_getpass.assert_not_called()
        assert mock_ssh_client.connect.call_count == 1


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


def test_run_tolerates_non_utf8_output(mock_ssh_client, mock_ssh_config, mock_path):
    """Non-UTF-8 bytes in command output must not blow up run().

    Command output is arbitrary bytes (Latin-1 locales, binary dumps, odd
    rpm metadata). The final decode used to be strict, so one bad byte
    raised UnicodeDecodeError out of run(); Target.run() swallowed it and
    recorded a successful command as failed (exitcode -1) with its output
    lost. Undecodable bytes must instead be replaced and the rest kept.
    """
    conn = Connection("test_host", 22, 300)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    mock_session.recv_exit_status.return_value = 0
    # \xe9 is Latin-1 'é', invalid as a UTF-8 sequence here.
    mock_session.recv.side_effect = [b"caf\xe9 ok\n", b""]
    mock_session.recv_stderr.side_effect = [b"warn\xff\n", b""]
    mock_session.recv_ready.side_effect = [True, False]
    mock_session.recv_stderr_ready.side_effect = [True, False]

    with patch("select.select", return_value=([mock_session], [], [])):
        exit_code = conn.run("rpm -qi weird-package")

    assert exit_code == 0
    assert conn.stdout == "caf� ok\n"
    assert conn.stderr == "warn�\n"


def test_run_closes_stdin_after_exec(mock_ssh_client, mock_ssh_config, mock_path):
    """run() must close the channel's write half so the remote command's
    stdin gets EOF and can't block forever waiting for input."""
    conn = Connection("test_host", 22, 300)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session
    mock_session.recv_exit_status.return_value = 0
    mock_session.recv.side_effect = [b"", b""]
    mock_session.recv_ready.side_effect = [False, False]
    mock_session.recv_stderr_ready.side_effect = [False, False]

    with patch("select.select", return_value=([mock_session], [], [])):
        conn.run("read x")

    mock_session.exec_command.assert_called_once_with("read x")
    mock_session.shutdown_write.assert_called_once_with()


def test_run_survives_shutdown_write_failure(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """A channel that can't shutdown_write must not break run()."""
    conn = Connection("test_host", 22, 300)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session
    mock_session.shutdown_write.side_effect = OSError("no write half")
    mock_session.recv_exit_status.return_value = 0
    mock_session.recv.side_effect = [b"ok", b""]
    mock_session.recv_ready.side_effect = [True, False]
    mock_session.recv_stderr_ready.side_effect = [False, False]

    with patch("select.select", return_value=([mock_session], [], [])):
        exit_code = conn.run("true")

    assert exit_code == 0
    assert conn.stdout == "ok"


def test_run_command_timeout(mock_ssh_client, mock_ssh_config, mock_path):
    """Timeout + callback returning 'n' must raise ``CommandTimeoutError``.

    Rewritten from the prior ``patch("builtins.input", ...)`` shape:
    the new ``Connection.__init__`` takes an injectable ``timeout_prompt``
    callable, so the test wires one directly instead of monkey-patching
    the global ``input`` builtin.
    """
    conn = Connection("test_host", 22, 300, timeout_prompt=lambda _text: "n")

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    with (
        patch("select.select", return_value=([], [], [])),
        pytest.raises(CommandTimeoutError),
    ):
        conn.run("sleep 10")


def test_run_command_timeout_wait_continues(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """Timeout + callback returning Enter (empty) must loop and complete.

    First ``select.select`` returns "no fds ready" (timeout); the
    callback returns ``""`` (Enter) which the parser treats as "wait".
    Second ``select.select`` returns the session ready so the recv
    loop terminates normally with exit code 0.
    """
    prompt_calls = []

    def _prompt(text: str) -> str:
        prompt_calls.append(text)
        return ""  # Enter == wait

    conn = Connection("test_host", 22, 300, timeout_prompt=_prompt)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session
    mock_session.recv_exit_status.return_value = 0
    mock_session.recv.side_effect = [b"done", b""]
    mock_session.recv_stderr.side_effect = [b""]
    mock_session.recv_ready.side_effect = [True, False]
    mock_session.recv_stderr_ready.side_effect = [False, False]

    # Three outer iterations: (1) select times out → continue,
    # (2) select ready → recv "done" → buffer non-empty → loop,
    # (3) select ready → no data → break.
    with patch(
        "select.select",
        side_effect=[
            ([], [], []),
            ([mock_session], [], []),
            ([mock_session], [], []),
        ],
    ):
        exit_code = conn.run("sleep 1")

    assert exit_code == 0
    assert len(prompt_calls) == 1
    assert "timed out" in prompt_calls[0]
    assert "test_host" in prompt_calls[0]


def test_run_command_timeout_no_callback_waits_silently(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """No callback wired: loop silently after a WARNING log line.

    Pins the default behaviour for library callers / scripts / tests
    that build a ``Connection`` without a prompter. The legacy code
    would have blocked on ``input()``; the new default emits a
    WARNING and continues to wait, preserving the Enter / Y default.
    """
    import logging

    conn = Connection("test_host", 22, 300)  # no timeout_prompt

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session
    mock_session.recv_exit_status.return_value = 0
    mock_session.recv.side_effect = [b"done", b""]
    mock_session.recv_stderr.side_effect = [b""]
    mock_session.recv_ready.side_effect = [True, False]
    mock_session.recv_stderr_ready.side_effect = [False, False]

    # Three outer iterations: (1) select times out → WARNING → continue,
    # (2) select ready → recv "done" → loop, (3) select ready → no data → break.
    with (
        caplog.at_level(logging.WARNING, logger="mtui.connection"),
        patch(
            "select.select",
            side_effect=[
                ([], [], []),
                ([mock_session], [], []),
                ([mock_session], [], []),
            ],
        ),
    ):
        exit_code = conn.run("sleep 1")

    assert exit_code == 0
    assert any(
        "timed out on test_host" in rec.message
        and "no prompt callback wired" in rec.message
        for rec in caplog.records
    ), [rec.message for rec in caplog.records]


def test_run_command_timeout_noninteractive_aborts(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """Non-interactive session (mtui-mcp): an inactivity timeout aborts with
    CommandTimeoutError instead of looping forever. Distinct from the
    interactive default, which waits silently."""
    conn = Connection("test_host", 22, 300, interactive=False)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    with (
        patch("select.select", return_value=([], [], [])),
        pytest.raises(CommandTimeoutError),
    ):
        conn.run("sleep infinity")


def test_run_command_timeout_callback_runs_on_calling_thread(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """The injected callback runs synchronously on ``Connection.run``'s thread.

    Pins the C6 invariant: ``Connection.run`` does NOT spawn a fresh
    thread for the prompt. Whatever serialisation the callback
    performs (the production :class:`Prompter` holds a lock) is
    therefore the only thing fencing concurrent worker prompts. If a
    future change accidentally re-introduced a worker-side prompt
    thread, this test would catch it.
    """
    import threading

    captured_thread_ident: list[int] = []

    def _prompt(_text: str) -> str:
        captured_thread_ident.append(threading.get_ident())
        return "n"  # abort

    conn = Connection("test_host", 22, 300, timeout_prompt=_prompt)

    mock_session = MagicMock()
    mock_ssh_client.get_transport.return_value.open_session.return_value = mock_session

    worker_thread_ident: list[int] = []

    def _drive() -> None:
        worker_thread_ident.append(threading.get_ident())
        with (
            patch("select.select", return_value=([], [], [])),
            pytest.raises(CommandTimeoutError),
        ):
            conn.run("sleep 10")

    t = threading.Thread(target=_drive)
    t.start()
    t.join(timeout=5)
    assert not t.is_alive(), "worker thread leaked"

    assert len(captured_thread_ident) == 1
    assert len(worker_thread_ident) == 1
    assert captured_thread_ident[0] == worker_thread_ident[0], (
        "callback must run on the same thread as Connection.run, not a "
        "fresh one spawned by Connection itself"
    )


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
    monkeypatch.setattr(
        "mtui.hosts.connection.connection.Path", lambda _path: fake_path
    )
    with caplog.at_level("WARNING", logger="mtui.connection"):
        Connection("test_host", 22, 300)
    assert any("denied" in r.message for r in caplog.records)


def test_connect_key_auth_failure_warns_and_logs_detail_at_debug(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """A key-auth failure surfaces one WARNING; the traceback stays at DEBUG.

    The connection layer emits a single actionable WARNING (set up SSH key
    auth) and keeps the exception detail/traceback at DEBUG so only one
    user-facing line is printed for a single failure. The exception then
    propagates to the caller (Target.connect), which reports the
    user-facing ConnectingTargetFailedMessage.
    """
    mock_ssh_client.connect.side_effect = paramiko.AuthenticationException

    with (
        patch("getpass.getpass") as mock_getpass,
        pytest.raises(paramiko.AuthenticationException),
        caplog.at_level("DEBUG", logger="mtui.connection"),
    ):
        Connection("test_host", 22, 300)

    mock_getpass.assert_not_called()
    # No password retry: only the single key-auth attempt happened.
    assert mock_ssh_client.connect.call_count == 1
    # Nothing surfaces above WARNING from the connection layer.
    assert not [r for r in caplog.records if r.levelno >= 40]
    warnings = [r for r in caplog.records if r.levelname == "WARNING"]
    assert any("key authentication" in r.getMessage().lower() for r in warnings)
    # The detail (with traceback) is available at DEBUG for --debug runs.
    debug_records = [r for r in caplog.records if r.levelname == "DEBUG"]
    assert any(r.exc_info is not None for r in debug_records)


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


def test_connect_oserror_is_logged_and_raised(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """A network-level OSError on the *initial* connect is logged AND re-raised.

    Re-raising lets the caller (Target.connect) report the clean
    ConnectingTargetFailedMessage instead of proceeding with a dead transport
    and crashing later in is_locked()/parse_system(). The reconnect path
    (quiet=True) still swallows it -- see the next test.
    """
    mock_ssh_client.connect.side_effect = OSError("network")
    with (
        caplog.at_level("ERROR", logger="mtui.connection"),
        pytest.raises(OSError, match="network"),
    ):
        Connection("test_host", 22, 300)
    assert any("No valid connection" in r.message for r in caplog.records)


def test_connect_oserror_quiet_logs_at_debug_not_error(
    mock_ssh_client, mock_ssh_config, mock_path, caplog
):
    """quiet=True downgrades the unreachable-host log from error to debug."""
    conn = Connection("h", 22, 300)
    mock_ssh_client.connect.side_effect = OSError("network")
    with caplog.at_level("DEBUG", logger="mtui.connection"):
        conn.connect(quiet=True)
    # No error-level "No valid connection" line ...
    assert not any(
        r.levelname == "ERROR" and "No valid connection" in r.message
        for r in caplog.records
    )
    # ... but a debug breadcrumb is still emitted.
    assert any(
        r.levelname == "DEBUG" and "No valid connection" in r.message
        for r in caplog.records
    )


def test_reconnect_retries_are_quiet(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch, caplog
):
    """Reconnect retries call connect(quiet=True) so failures don't log as errors."""
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = False
    mock_ssh_client.connect.side_effect = OSError("still down")
    monkeypatch.setattr(
        "mtui.hosts.connection.connection.select.select",
        lambda *_a, **_kw: ([], [], []),
    )
    with (
        caplog.at_level("DEBUG", logger="mtui.connection"),
        pytest.raises(ConnectionError, match="Failed to reconnect"),
    ):
        conn.reconnect(retry=2, timeout=0)
    # The expected mid-reboot failures are debug, not error; only the final
    # ConnectionError (raised above) surfaces the give-up to the user.
    assert not any(
        r.levelname == "ERROR" and "No valid connection" in r.message
        for r in caplog.records
    )


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
        "mtui.hosts.connection.connection.select.select",
        lambda *_a, **_kw: ([], [], []),
    )
    with pytest.raises(ConnectionError, match="Failed to reconnect"):
        conn.reconnect(retry=2, timeout=0)


def test_fire_and_forget_dispatches_and_closes(
    mock_ssh_client, mock_ssh_config, mock_path
):
    """fire_and_forget sends the command on a session, then closes the link."""
    conn = Connection("h", 22, 300)
    session = MagicMock()
    # Connection has __slots__, so patch on the class, not the instance.
    with (
        patch.object(Connection, "new_session", return_value=session),
        patch.object(Connection, "close") as close,
    ):
        conn.fire_and_forget("systemctl reboot")

    session.exec_command.assert_called_once_with("systemctl reboot")
    close.assert_called_once()


def test_reconnect_backoff_grows_timeout(
    mock_ssh_client, mock_ssh_config, mock_path, monkeypatch
):
    """The backoff branch widens the select timeout per attempt."""
    conn = Connection("h", 22, 300)
    mock_ssh_client._transport.is_active.return_value = False
    sel_mock = MagicMock(return_value=([], [], []))
    monkeypatch.setattr("mtui.hosts.connection.connection.select.select", sel_mock)
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
    from mtui.support.messages import ReConnectFailed

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
    with patch(
        "mtui.hosts.connection.connection.select.select", return_value=([sess], [], [])
    ):
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
    from mtui.support.messages import ReConnectFailed

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


def test_sftp_put_propagates_paramiko_errors_without_reconnect(
    conn_with_sftp, sftp_client, mock_ssh_client
):
    """Paramiko transport errors during ``mkdir`` propagate to the caller.

    Regression: ``sftp_put`` used to rebind the local ``sftp`` to a fresh
    client returned from ``__sftp_reconnect`` on transport errors. Because
    ``_sftp``'s ``finally`` closes the original (captured) client, the
    rebound client was never closed and leaked. The fix drops the inner
    retry loop entirely; any paramiko error propagates instead, and the
    single SFTP client opened by ``_sftp`` is closed exactly once.
    """
    from pathlib import Path as _Path

    sftp_client.mkdir.side_effect = paramiko.SSHException("transport gone")
    with pytest.raises(paramiko.SSHException):
        conn_with_sftp.sftp_put(_Path("/local"), _Path("/srv/sub/file.txt"))
    # No retry => the put never happens.
    sftp_client.put.assert_not_called()
    # Exactly one SFTPClient was opened and exactly one was closed:
    # nothing leaked from a mid-loop reconnect.
    assert mock_ssh_client.open_sftp.call_count == 1
    assert sftp_client.close.call_count == 1


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


# ---------------------------------------------------------------------------
# sftp_session() context manager + multi-step internal reuse (C7)
# ---------------------------------------------------------------------------


def test_sftp_session_reuses_single_client(
    conn_with_sftp, sftp_client, mock_ssh_client
):
    """Multiple ops inside one ``sftp_session`` share the same client."""
    with conn_with_sftp.sftp_session() as sftp:
        sftp.listdir("/etc")
        sftp.readlink("/some/link")
        sftp.listdir("/var")

    # One handshake at entry, one close on exit; no per-op churn.
    assert mock_ssh_client.open_sftp.call_count == 1
    assert sftp_client.close.call_count == 1


def test_sftp_session_propagates_paramiko_errors(
    conn_with_sftp, sftp_client, mock_ssh_client
):
    """Mid-block paramiko errors propagate; the client is still closed."""
    sftp_client.listdir.side_effect = paramiko.SSHException("broken")

    with (  # noqa: PT012
        pytest.raises(paramiko.SSHException, match="broken"),
        conn_with_sftp.sftp_session() as sftp,
    ):
        sftp.listdir("/")
        # Second op never runs.
        sftp.readlink("/nope")

    # No auto-retry: a single handshake, single close.
    assert mock_ssh_client.open_sftp.call_count == 1
    assert sftp_client.close.call_count == 1
    sftp_client.readlink.assert_not_called()


def test_sftp_get_folder_uses_one_session(conn_with_sftp, sftp_client, mock_ssh_client):
    """``sftp_get_folder`` performs listdir + N gets over a single session."""
    from pathlib import Path as _Path

    sftp_client.listdir.return_value = ["a", "b", "c"]
    conn_with_sftp.sftp_get_folder(_Path("/remote"), _Path("/local/"))

    # 1 handshake for the whole batch (listdir + 3 gets), not 4.
    assert mock_ssh_client.open_sftp.call_count == 1
    sftp_client.listdir.assert_called_once_with("/remote")
    assert sftp_client.get.call_count == 3
    assert sftp_client.close.call_count == 1


def test_sftp_rmdir_uses_one_session(conn_with_sftp, sftp_client, mock_ssh_client):
    """``sftp_rmdir`` performs listdir + N removes + rmdir over a single session."""
    from pathlib import Path as _Path

    sftp_client.listdir.return_value = ["a", "b"]
    conn_with_sftp.sftp_rmdir(_Path("/some/dir"))

    # Pre-C7 this opened 1 (listdir) + 2 (remove) + 1 (rmdir) = 4 sessions.
    assert mock_ssh_client.open_sftp.call_count == 1
    sftp_client.listdir.assert_called_once_with("/some/dir")
    assert sftp_client.remove.call_count == 2
    sftp_client.rmdir.assert_called_once_with("/some/dir")
    assert sftp_client.close.call_count == 1
