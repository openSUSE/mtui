"""Tests for the `lrun` command."""

from __future__ import annotations

import io
import logging
from argparse import Namespace
from subprocess import CompletedProcess
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.localrun import LocalRun


def _prompt(interactive: bool = True) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    p.interactive = interactive
    return p


def _fake_sys() -> SimpleNamespace:
    """Build a minimal sys-like object with real StringIO buffers."""

    def _exit(code: int = 0) -> None:
        raise SystemExit(code)

    return SimpleNamespace(
        stdout=io.StringIO(),
        stderr=io.StringIO(),
        argv=["mtui-mcp"],
        exit=_exit,
    )


def test_lrun_interactive_uses_check_call(mock_config):
    prompt = _prompt(interactive=True)
    args = Namespace(command=["echo", "hi"])
    with (
        patch("mtui.commands.localrun.subprocess.check_call") as cc,
        patch("mtui.commands.localrun.subprocess.run") as run,
    ):
        LocalRun(args, mock_config, MagicMock(), prompt)()
    cc.assert_called_once_with("echo hi", shell=True)
    run.assert_not_called()


def test_lrun_empty_command_logs_error(mock_config, caplog):
    prompt = _prompt(interactive=True)
    args = Namespace(command=[])
    caplog.set_level(logging.ERROR, logger="mtui.commands.lrun")
    with patch("mtui.commands.localrun.subprocess.check_call") as cc:
        LocalRun(args, mock_config, MagicMock(), prompt)()
    cc.assert_not_called()
    assert any("Missing argument" in r.message for r in caplog.records)


def test_lrun_noninteractive_captures_stdout(mock_config):
    prompt = _prompt(interactive=False)
    args = Namespace(command=["echo", "hello"])
    fake = _fake_sys()
    with patch("mtui.commands.localrun.subprocess.run") as run:
        run.return_value = CompletedProcess(
            args="echo hello", returncode=0, stdout="hello\n", stderr=""
        )
        LocalRun(args, mock_config, fake, prompt)()
    run.assert_called_once_with(
        "echo hello",
        shell=True,
        capture_output=True,
        text=True,
        check=False,
    )
    assert fake.stdout.getvalue() == "hello\n"
    assert fake.stderr.getvalue() == ""


def test_lrun_noninteractive_captures_stderr_and_exits(mock_config):
    prompt = _prompt(interactive=False)
    args = Namespace(command=["refdb", "12.SP5"])
    fake = _fake_sys()
    with patch("mtui.commands.localrun.subprocess.run") as run:
        run.return_value = CompletedProcess(
            args="refdb 12.SP5",
            returncode=127,
            stdout="",
            stderr="refdb: command not found\n",
        )
        with pytest.raises(SystemExit) as excinfo:
            LocalRun(args, mock_config, fake, prompt)()
    assert excinfo.value.code == 127
    assert fake.stderr.getvalue() == "refdb: command not found\n"
    assert fake.stdout.getvalue() == ""
