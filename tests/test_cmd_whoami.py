"""Tests for the `whoami` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.whoami import Whoami


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_whoami_happy(mock_config):
    sys_mock = MagicMock()
    mock_config.session_user = "alice"
    prompt = _prompt()

    with patch.object(Whoami, "get_pid", return_value=4242):
        Whoami(Namespace(), mock_config, sys_mock, prompt)()

    sys_mock.stdout.write.assert_called_once_with("User: alice, app pid: 4242\n")


def test_whoami_calls_get_pid_when_not_patched(mock_config):
    """Smoke: the real get_pid returns an int and the line still renders."""
    sys_mock = MagicMock()
    mock_config.session_user = "bob"
    prompt = _prompt()

    Whoami(Namespace(), mock_config, sys_mock, prompt)()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert written.startswith("User: bob, app pid: ")
