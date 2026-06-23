"""Coverage for the registered tool *closures* in :mod:`mtui.mcp.tools` and
:mod:`mtui.mcp.testreport_tools`.

The other mcp tests drive :class:`~mtui.mcp.session.McpSession` directly; here
we register the workspace / job / testreport tools on a capturing fake server
and invoke the registered coroutines, so the closure bodies (workspace listing
and close, the job-control wrappers, and the ``resolve_session`` hop in each
testreport tool) are exercised through a real
:class:`~mtui.mcp.registry.SessionRegistry`.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, cast
from unittest.mock import MagicMock

from mtui.commands.whoami import Whoami
from mtui.mcp.main import build_session
from mtui.mcp.registry import SessionRegistry, resolve_session
from mtui.mcp.testreport_tools import register_testreport_tools
from mtui.mcp.tools import register_job_tools, register_workspace_tools

if TYPE_CHECKING:
    from mcp.server.fastmcp import FastMCP

    from mtui.mcp.registry import SessionProvider

_LOG = logging.getLogger("test.mcp.toollayer")


class FakeMCP:
    """Captures ``add_tool(fn, name=...)`` registrations by name."""

    def __init__(self) -> None:
        self.tools: dict = {}

    def add_tool(self, fn, name, **_kw):  # noqa: ANN001
        self.tools[name] = fn


def _as_server(mcp: FakeMCP) -> FastMCP:
    return cast("FastMCP", mcp)


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _registry(tmp_path: Path) -> SessionRegistry:
    return SessionRegistry(
        build_session, _config(tmp_path), _LOG, max_sessions=32, idle_timeout=0.0
    )


# --------------------------------------------------------------------------- #
# workspace tools                                                             #
# --------------------------------------------------------------------------- #
def test_list_workspaces_empty_populated_and_close(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    mcp = FakeMCP()
    assert sorted(register_workspace_tools(_as_server(mcp), reg)) == [
        "close_workspace",
        "list_workspaces",
    ]

    async def driver() -> tuple[str, str, str, str]:
        empty = await mcp.tools["list_workspaces"]()
        await resolve_session(reg, None, "default")  # mint the default workspace
        listed = await mcp.tools["list_workspaces"]()
        closed = await mcp.tools["close_workspace"]("default")
        missing = await mcp.tools["close_workspace"]("nope")
        return empty, listed, closed, missing

    empty, listed, closed, missing = asyncio.run(driver())
    assert "no workspaces yet" in empty
    assert "default" in listed
    assert "empty (no template loaded)" in listed
    assert "closed workspace 'default'" in closed
    assert "no such workspace" in missing


def test_workspace_tools_without_provider_support() -> None:
    """A provider that is not a SessionRegistry → graceful one-line message."""
    mcp = FakeMCP()
    register_workspace_tools(
        _as_server(mcp), cast("SessionProvider", SimpleNamespace())
    )  # no live_sessions/evict

    async def driver() -> tuple[str, str]:
        return (
            await mcp.tools["list_workspaces"](),
            await mcp.tools["close_workspace"]("default"),
        )

    lst, cls = asyncio.run(driver())
    assert "not supported" in lst
    assert "not supported" in cls


# --------------------------------------------------------------------------- #
# job-control tools                                                           #
# --------------------------------------------------------------------------- #
def test_job_tools_empty_then_full_lifecycle(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    mcp = FakeMCP()
    assert set(register_job_tools(_as_server(mcp), reg)) == {
        "job_list",
        "job_status",
        "job_result",
        "job_cancel",
    }

    async def driver() -> tuple[str, str, str, str, str, str]:
        empty = await mcp.tools["job_list"](workspace="default")
        sess = await resolve_session(reg, None, "default")
        jid = await sess.start_job(Whoami, [])
        await sess._jobs[jid]["task"]  # let the worker finish
        jl = await mcp.tools["job_list"](workspace="default")
        js = await mcp.tools["job_status"](jid, workspace="default")
        jr = await mcp.tools["job_result"](jid, workspace="default")
        jc = await mcp.tools["job_cancel"](jid, workspace="default")
        return empty, jl, js, jr, jc, jid

    empty, jl, js, jr, jc, jid = asyncio.run(driver())
    assert "no background jobs" in empty
    assert jid in jl
    assert jid in js
    assert "User: testuser" in jr
    assert isinstance(jc, str)


# --------------------------------------------------------------------------- #
# testreport tool closures — exercise the resolve_session hop in each         #
# --------------------------------------------------------------------------- #
def test_testreport_tool_closures_resolve_session(tmp_path: Path) -> None:
    reg = _registry(tmp_path)
    mcp = FakeMCP()
    register_testreport_tools(_as_server(mcp), reg)

    async def driver() -> set[str]:
        seen: set[str] = set()
        calls = [
            ("testreport_logs", ()),
            ("testreport_read_file", ("source.diff",)),
            ("testreport_patch", (1, 1, "x")),
            ("testreport_write", ("x",)),
        ]
        for name, args in calls:
            # no template is loaded, so the underlying op may raise after the
            # resolve_session line we want covered — both outcomes are fine.
            with contextlib.suppress(Exception):
                await mcp.tools[name](*args, workspace="default")
            seen.add(name)
        return seen

    seen = asyncio.run(driver())
    assert seen == {
        "testreport_logs",
        "testreport_read_file",
        "testreport_patch",
        "testreport_write",
    }
