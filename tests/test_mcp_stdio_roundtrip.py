"""End-to-end in-memory roundtrip smoke test for ``mtui-mcp``.

Uses the official SDK's
:func:`mcp.shared.memory.create_connected_server_and_client_session`
helper — no subprocess, no socket — to prove the wiring in
:mod:`mtui.mcp.main` actually produces a working MCP server: tools
list reflects the deny-list, the three testreport tools are present,
and a real auto-generated tool (``whoami``) round-trips to the same
``User: ...`` line the REPL emits.

Skipped when the ``[mcp]`` extra is not installed.
"""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from unittest.mock import MagicMock

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402
from mcp.shared.memory import (  # noqa: E402
    create_connected_server_and_client_session,
)

from mtui.mcp.session import McpSession  # noqa: E402
from mtui.mcp.testreport_tools import register_testreport_tools  # noqa: E402
from mtui.mcp.tools import build_tools  # noqa: E402


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _build_server(tmp_path: Path) -> FastMCP:
    mcp: FastMCP = FastMCP(name="mtui-test")
    session = McpSession(_config(tmp_path), logging.getLogger("test.mcp.roundtrip"))
    build_tools(mcp, session)
    register_testreport_tools(mcp, session)
    return mcp


def _result_text(result) -> str:
    """Pull the human-readable text out of a :class:`mcp.types.CallToolResult`.

    The SDK returns a ``CallToolResult`` whose ``content`` is a list
    of typed content blocks. We accept either ``.text`` on the first
    block or fall back to ``str(result)`` so the assert remains useful
    if the envelope shape shifts between minor versions.
    """
    content = getattr(result, "content", None)
    if content:
        first = content[0]
        text = getattr(first, "text", None)
        if text is not None:
            return text
    return str(result)


def test_inmemory_roundtrip_lists_tools(tmp_path: Path) -> None:
    """The in-memory client sees the synthesised tool set."""
    mcp = _build_server(tmp_path)

    async def driver() -> set[str]:
        async with create_connected_server_and_client_session(server=mcp) as session:
            await session.initialize()
            result = await session.list_tools()
            return {t.name for t in result.tools}

    names = asyncio.run(driver())

    # Auto-generated tools from non-denied commands.
    assert "whoami" in names
    # Deny-listed commands must not appear.
    assert "edit" not in names
    assert "terms" not in names
    assert "shell" not in names
    assert "help" not in names
    assert "quit" not in names
    assert "exit" not in names
    # The three hand-written testreport tools must all be present.
    assert "testreport_read" in names
    assert "testreport_patch" in names
    assert "testreport_write" in names


def test_inmemory_roundtrip_calls_whoami(tmp_path: Path) -> None:
    """``call_tool('whoami')`` returns the same banner the REPL prints."""
    mcp = _build_server(tmp_path)

    async def driver() -> str:
        async with create_connected_server_and_client_session(server=mcp) as session:
            await session.initialize()
            return _result_text(await session.call_tool("whoami", {}))

    text = asyncio.run(driver())
    assert text.startswith("User: testuser, app pid: ")
