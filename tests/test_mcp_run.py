"""Tests for the `run` command under the MCP transport.

The bug: ``Run.__call__`` historically emitted its per-host results via
``page(output, self.prompt.interactive)``. The pager early-returned in
non-interactive mode, leaving ``McpSession``'s per-call ``StringIO``
empty and handing back ``""`` to the MCP client.

The fix routes non-interactive output through the caller's
``display.println``, which writes into the captured ``StringIO`` so the
MCP client actually receives the per-host result lines.

These tests pin that contract: drive ``McpSession.run_command`` with a
mocked ``HostsGroup`` and assert the returned string carries the
expected per-host header, stdout, and stderr lines.
"""

from __future__ import annotations

import asyncio
import logging
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import MagicMock, patch

from mtui.commands.run import Run
from mtui.mcp.session import McpSession


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _make_session(tmp_path: Path) -> McpSession:
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.run"))


def _fake_target(stdout: str = "Linux host1\n", stderr: str = "") -> MagicMock:
    """Build a target double whose ``last*`` methods return canned values."""
    t = MagicMock()
    t.hostname = "host1"
    t.state = "enabled"
    t.lastin.return_value = "uname -a"
    t.lastexit.return_value = 0
    t.lastout.return_value = stdout
    t.lasterr.return_value = stderr
    return t


@contextmanager
def _noop_lock(_targets):
    yield


def test_mcp_run_returns_per_host_output(tmp_path: Path) -> None:
    """``run`` under MCP must return the per-host result lines to the client.

    Pins the regression: pre-fix this returned ``""`` because ``page()``
    early-returned in non-interactive mode and the captured StringIO
    stayed empty.
    """
    sess = _make_session(tmp_path)
    target = _fake_target(stdout="Linux host1 6.10.0\n")

    # ``Run`` reads its target mapping from ``self.parse_hosts``; stub
    # that to return a HostsGroup-shaped MagicMock so the command runs
    # against our single canned target.
    hg = MagicMock()
    hg.values.return_value = [target]
    hg.__iter__ = lambda self: iter(["host1"])
    hg.__getitem__ = lambda self, key: target
    hg.__bool__ = lambda self: True

    with (
        patch("mtui.commands.run.LockedTargets", _noop_lock),
        patch.object(Run, "parse_hosts", return_value=hg),
    ):
        out = asyncio.run(sess.run_command(Run, ["uname", "-a"]))

    hg.run.assert_called_once_with("uname -a")
    assert "host1:-> uname -a [0]" in out
    assert "Linux host1 6.10.0" in out


def test_mcp_run_includes_stderr_block(tmp_path: Path) -> None:
    """Non-empty stderr is surfaced under a ``stderr:`` header."""
    sess = _make_session(tmp_path)
    target = _fake_target(stdout="", stderr="permission denied\n")

    hg = MagicMock()
    hg.values.return_value = [target]
    hg.__iter__ = lambda self: iter(["host1"])
    hg.__getitem__ = lambda self, key: target
    hg.__bool__ = lambda self: True

    with (
        patch("mtui.commands.run.LockedTargets", _noop_lock),
        patch.object(Run, "parse_hosts", return_value=hg),
    ):
        out = asyncio.run(sess.run_command(Run, ["uname"]))

    assert "stderr:" in out
    assert "permission denied" in out
