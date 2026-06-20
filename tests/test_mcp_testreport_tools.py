"""Tests for :mod:`mtui.mcp.testreport_tools`.

Covers the eight behaviours enumerated in PLAN.md step 9 plus the
registration smoke test:

* refusal-without-load for ``testreport_read``
* round-trip read returns the file contents and an accurate line count
* ``testreport_patch`` replaces an inclusive line range
* ``testreport_patch`` inserts before the first line
* ``testreport_patch`` rejects out-of-range arguments with a message
  that names the actual line count
* ``testreport_patch`` leaves the original file untouched and the
  parent directory ``tmp*``-free when :func:`os.replace` fails
* ``testreport_write`` overwrites atomically and the byte count
  round-trips
* :func:`register_testreport_tools` exposes the three tools with the
  expected ``readOnlyHint`` shape

The synchronous wrapping mirrors ``tests/test_mcp_session.py``: each
async tool is driven through :func:`asyncio.run` because the dev group
does not pull in ``pytest-asyncio``.
"""

from __future__ import annotations

import asyncio
import logging
import os
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.mcp import testreport_tools as tt
from mtui.mcp.session import McpCommandError, McpSession

# --------------------------------------------------------------------------- #
# Fixtures                                                                    #
# --------------------------------------------------------------------------- #


def _config(tmp_path: Path) -> MagicMock:
    """Same minimal Config used by ``tests/test_mcp_session.py``."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _null_session(tmp_path: Path) -> McpSession:
    """Real :class:`McpSession` whose metadata is the default NullTestReport."""
    return McpSession(_config(tmp_path), logging.getLogger("test.mcp.testreport"))


def _loaded_session(tmp_path: Path, path: Path) -> SimpleNamespace:
    """Fake session that only needs the two attributes the tools touch.

    Avoids constructing a full TestReport — we just need ``metadata.path``
    to be a non-None string and ``metadata`` to *not* be a NullTestReport.
    ``_lock`` is a real :class:`asyncio.Lock` so the ``async with`` works.
    """
    metadata = SimpleNamespace(path=str(path))
    return SimpleNamespace(metadata=metadata, _lock=asyncio.Lock())


# --------------------------------------------------------------------------- #
# Refusal when no testreport is loaded                                        #
# --------------------------------------------------------------------------- #


def test_read_refuses_without_loaded_report(tmp_path: Path) -> None:
    """``testreport_read`` raises a clear error on a NullTestReport session."""
    sess = _null_session(tmp_path)
    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_read(sess))
    assert ei.value.exit_code == 1
    assert "load_template" in ei.value.stderr


def test_patch_refuses_without_loaded_report(tmp_path: Path) -> None:
    sess = _null_session(tmp_path)
    with pytest.raises(McpCommandError):
        asyncio.run(tt.testreport_patch(sess, 1, 1, "x\n"))


def test_write_refuses_without_loaded_report(tmp_path: Path) -> None:
    sess = _null_session(tmp_path)
    with pytest.raises(McpCommandError):
        asyncio.run(tt.testreport_write(sess, "x"))


def test_resolve_returns_path_for_loaded_session(tmp_path: Path) -> None:
    """Sanity-check the helper used by all three tools."""
    file = tmp_path / "report.txt"
    file.write_text("ok\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)
    assert tt._resolve_testreport_path(sess) == file  # ty: ignore[invalid-argument-type]


# --------------------------------------------------------------------------- #
# Read                                                                        #
# --------------------------------------------------------------------------- #


def test_read_returns_file_contents(tmp_path: Path) -> None:
    """A 5-line tmp file produces ``line_count == 5`` and matching content."""
    file = tmp_path / "report.txt"
    body = "a\nb\nc\nd\ne\n"
    file.write_text(body, encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result: dict[str, Any] = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]

    assert result["path"] == str(file)
    assert result["line_count"] == 5
    assert result["content"] == body
    # Default (no window) reply keeps its original shape.
    assert "offset" not in result
    assert "returned_lines" not in result


def test_read_window_offset_and_limit(tmp_path: Path) -> None:
    """``offset``/``limit`` return a 1-indexed line window (matches patch numbering)."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\nd\ne\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result: dict[str, Any] = asyncio.run(
        tt.testreport_read(sess, offset=2, limit=2)  # ty: ignore[invalid-argument-type]
    )

    assert result["content"] == "b\nc\n"
    assert result["line_count"] == 5  # total, not the window size
    assert result["offset"] == 2
    assert result["returned_lines"] == 2


def test_read_window_offset_to_end(tmp_path: Path) -> None:
    """``offset`` with no ``limit`` reads to end of file."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\nd\ne\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result: dict[str, Any] = asyncio.run(
        tt.testreport_read(sess, offset=4)  # ty: ignore[invalid-argument-type]
    )

    assert result["content"] == "d\ne\n"
    assert result["returned_lines"] == 2
    assert result["line_count"] == 5


def test_read_window_offset_past_end_is_empty(tmp_path: Path) -> None:
    """An offset beyond the file yields empty content, not an error."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result: dict[str, Any] = asyncio.run(
        tt.testreport_read(sess, offset=99)  # ty: ignore[invalid-argument-type]
    )

    assert result["content"] == ""
    assert result["returned_lines"] == 0
    assert result["line_count"] == 2


def test_read_window_rejects_bad_offset_and_limit(tmp_path: Path) -> None:
    """``offset < 1`` and ``limit < 0`` are rejected with McpCommandError."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    with pytest.raises(tt.McpCommandError):
        asyncio.run(tt.testreport_read(sess, offset=0))  # ty: ignore[invalid-argument-type]
    with pytest.raises(tt.McpCommandError):
        asyncio.run(tt.testreport_read(sess, limit=-1))  # ty: ignore[invalid-argument-type]


# --------------------------------------------------------------------------- #
# Patch                                                                       #
# --------------------------------------------------------------------------- #


def test_patch_replaces_range(tmp_path: Path) -> None:
    """Replace lines 2–3 of a 5-line file with three new lines."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\nd\ne\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result = asyncio.run(tt.testreport_patch(sess, 2, 3, "X\nY\nZ\n"))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nX\nY\nZ\nd\ne\n"
    assert result["replaced_lines"] == 2
    assert result["inserted_lines"] == 3
    assert result["new_line_count"] == 6


def test_patch_insert_before_first_line(tmp_path: Path) -> None:
    """``start_line=1, end_line=0`` prepends without removing anything."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result = asyncio.run(tt.testreport_patch(sess, 1, 0, "HDR\n"))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "HDR\na\nb\n"
    assert result["replaced_lines"] == 0
    assert result["inserted_lines"] == 1
    assert result["new_line_count"] == 3


def test_patch_normalises_missing_trailing_newline(tmp_path: Path) -> None:
    """Replacement without trailing ``\\n`` still keeps the splice glued correctly."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    asyncio.run(tt.testreport_patch(sess, 2, 2, "MID"))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nMID\nc\n"


def test_patch_empty_replacement_is_pure_delete(tmp_path: Path) -> None:
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    result = asyncio.run(tt.testreport_patch(sess, 2, 2, ""))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nc\n"
    assert result["replaced_lines"] == 1
    assert result["inserted_lines"] == 0
    assert result["new_line_count"] == 2


def test_patch_out_of_range_errors_name_the_line_count(tmp_path: Path) -> None:
    """The error message must mention the actual line count."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_patch(sess, 10, 12, "x\n"))  # ty: ignore[invalid-argument-type]
    assert "3" in ei.value.stderr  # the actual line count

    with pytest.raises(McpCommandError):
        # start_line < 1
        asyncio.run(tt.testreport_patch(sess, 0, 1, "x\n"))  # ty: ignore[invalid-argument-type]

    with pytest.raises(McpCommandError):
        # end_line < start_line-1
        asyncio.run(tt.testreport_patch(sess, 2, 0, "x\n"))  # ty: ignore[invalid-argument-type]


def test_patch_is_atomic_on_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A simulated ``os.replace`` failure leaves the file and dir clean."""
    file = tmp_path / "report.txt"
    original = "a\nb\nc\n"
    file.write_text(original, encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    def boom(*_args: object, **_kwargs: object) -> None:
        raise OSError("simulated replace failure")

    # Patch the symbol used inside testreport_tools' module namespace.
    import mtui.mcp.testreport_tools as mod

    monkeypatch.setattr(mod.os, "replace", boom)

    with pytest.raises(OSError, match="simulated replace failure"):
        asyncio.run(tt.testreport_patch(sess, 1, 1, "X\n"))  # ty: ignore[invalid-argument-type]

    # Original file unchanged.
    assert file.read_text(encoding="utf-8") == original
    # No tmp residue. Hidden tmp files start with ".report.txt." in our dir.
    leftovers = [
        p.name for p in tmp_path.iterdir() if p.name != "report.txt" and p.is_file()
    ]
    assert leftovers == [], f"unexpected tmp residue: {leftovers!r}"


# --------------------------------------------------------------------------- #
# Write                                                                       #
# --------------------------------------------------------------------------- #


def test_write_overwrites_atomically(tmp_path: Path) -> None:
    """Byte count matches the encoded payload; file content matches on read-back."""
    file = tmp_path / "report.txt"
    file.write_text("old\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)

    new_content = "line1\nline2\n"
    result = asyncio.run(tt.testreport_write(sess, new_content))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == new_content
    assert result["bytes_written"] == len(new_content.encode("utf-8"))
    assert result["line_count"] == 2
    assert result["path"] == str(file)


def test_atomic_write_text_unlinks_tmp_on_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Direct helper test: ``os.replace`` failure must unlink the tmpfile."""
    file = tmp_path / "report.txt"
    file.write_text("orig\n", encoding="utf-8")

    def boom(*_args: object, **_kwargs: object) -> None:
        raise OSError("nope")

    monkeypatch.setattr(os, "replace", boom)

    with pytest.raises(OSError, match="nope"):
        tt._atomic_write_text(file, "new\n")

    assert file.read_text(encoding="utf-8") == "orig\n"
    leftovers = [p.name for p in tmp_path.iterdir() if p.name != "report.txt"]
    assert leftovers == []


# --------------------------------------------------------------------------- #
# Registration                                                                #
# --------------------------------------------------------------------------- #


def test_register_testreport_tools_exposes_all_tools(tmp_path: Path) -> None:
    """Smoke-test: all tools registered, ``readOnlyHint`` set as expected."""
    pytest.importorskip("mcp")
    from mcp.server.fastmcp import FastMCP

    sess = _null_session(tmp_path)
    mcp: FastMCP = FastMCP(name="test-mtui-mcp")

    names = tt.register_testreport_tools(mcp, sess)

    assert names == [
        "testreport_logs",
        "testreport_patch",
        "testreport_read",
        "testreport_read_file",
        "testreport_write",
    ]

    tools: dict[str, Any] = {}
    for n in names:
        tool = mcp._tool_manager.get_tool(n)  # noqa: SLF001
        assert tool is not None, f"tool {n!r} missing after registration"
        tools[n] = tool

    # The read-only tools advertise it; the mutating ones must not.
    for n in ("testreport_read", "testreport_logs", "testreport_read_file"):
        assert tools[n].annotations is not None
        assert tools[n].annotations.readOnlyHint is True
        assert tools[n].annotations.idempotentHint is True
    for n in ("testreport_patch", "testreport_write"):
        if tools[n].annotations is not None:
            assert tools[n].annotations.readOnlyHint is not True

    # The read-first warning is glued onto each edit-flow description.
    for n in ("testreport_read", "testreport_patch", "testreport_write"):
        assert "testreport_read" in tools[n].description


# --------------------------------------------------------------------------- #
# Auxiliary checkout files: testreport_logs / testreport_read_file            #
# --------------------------------------------------------------------------- #


def test_logs_and_read_file_roundtrip(tmp_path: Path) -> None:
    """``testreport_logs`` inventories the subdirs; ``read_file`` returns one."""
    (tmp_path / "build_checks").mkdir()
    (tmp_path / "install_logs").mkdir()
    (tmp_path / "build_checks" / "libica.s390x.log").write_text("ok\nfine\n")
    (tmp_path / "install_logs" / "host1.log").write_text("zypper\n")
    sess = _loaded_session(tmp_path, tmp_path / "log")

    logs = asyncio.run(tt.testreport_logs(sess))  # ty: ignore[invalid-argument-type]
    assert [f["name"] for f in logs["build_checks"]] == ["libica.s390x.log"]
    assert [f["name"] for f in logs["install_logs"]] == ["host1.log"]

    out = asyncio.run(
        tt.testreport_read_file(sess, "build_checks/libica.s390x.log")  # ty: ignore[invalid-argument-type]
    )
    assert out["content"] == "ok\nfine\n"
    assert out["line_count"] == 2


def test_logs_empty_when_subdirs_absent(tmp_path: Path) -> None:
    sess = _loaded_session(tmp_path, tmp_path / "log")
    logs = asyncio.run(tt.testreport_logs(sess))  # ty: ignore[invalid-argument-type]
    assert logs["build_checks"] == []
    assert logs["install_logs"] == []


def test_read_file_missing_raises(tmp_path: Path) -> None:
    sess = _loaded_session(tmp_path, tmp_path / "log")
    with pytest.raises(McpCommandError, match="no such file"):
        asyncio.run(tt.testreport_read_file(sess, "build_checks/nope.log"))  # ty: ignore[invalid-argument-type]


def test_read_file_rejects_traversal(tmp_path: Path) -> None:
    """A relative path escaping the checkout dir is refused."""
    (tmp_path.parent / "secret.txt").write_text("nope\n")
    sess = _loaded_session(tmp_path, tmp_path / "log")
    with pytest.raises(McpCommandError, match="escapes"):
        asyncio.run(tt.testreport_read_file(sess, "../secret.txt"))  # ty: ignore[invalid-argument-type]


def test_logs_and_read_file_refuse_without_loaded_report(tmp_path: Path) -> None:
    sess = _null_session(tmp_path)
    with pytest.raises(McpCommandError):
        asyncio.run(tt.testreport_logs(sess))
    with pytest.raises(McpCommandError):
        asyncio.run(tt.testreport_read_file(sess, "source.diff"))


# --------------------------------------------------------------------------- #
# Progress-notification heartbeat                                             #
# --------------------------------------------------------------------------- #


class _RecordingCtx:
    """Stand-in for :class:`mcp.server.fastmcp.Context`.

    Mirrors the one in ``tests/test_mcp_session_progress.py`` but
    duplicated locally to keep the two test modules independent.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[float, float | None, str | None]] = []

    async def report_progress(
        self,
        progress: float,
        total: float | None = None,
        message: str | None = None,
    ) -> None:
        self.calls.append((progress, total, message))


def test_testreport_read_emits_heartbeat_when_ctx_supplied(tmp_path: Path) -> None:
    """When called with a Context, ``testreport_read`` fires one progress frame."""
    file = tmp_path / "report.txt"
    file.write_text("x\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)
    ctx = _RecordingCtx()

    result = asyncio.run(tt.testreport_read(sess, ctx=ctx))  # ty: ignore[invalid-argument-type]

    assert result["content"] == "x\n"
    assert len(ctx.calls) == 1
    _progress, _total, message = ctx.calls[0]
    assert message is not None
    assert "testreport_read" in message


def test_testreport_write_emits_heartbeat_when_ctx_supplied(tmp_path: Path) -> None:
    """``testreport_write`` also fires one progress frame for client patience."""
    file = tmp_path / "report.txt"
    file.write_text("orig\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)
    ctx = _RecordingCtx()

    asyncio.run(tt.testreport_write(sess, "new\n", ctx=ctx))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "new\n"
    assert len(ctx.calls) == 1


def test_testreport_patch_emits_heartbeat_when_ctx_supplied(tmp_path: Path) -> None:
    """``testreport_patch`` fires one progress frame before the splice."""
    file = tmp_path / "report.txt"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _loaded_session(tmp_path, file)
    ctx = _RecordingCtx()

    asyncio.run(tt.testreport_patch(sess, 2, 2, "B\n", ctx=ctx))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nB\nc\n"
    assert len(ctx.calls) == 1


def test_testreport_tools_strip_ctx_from_json_schema(tmp_path: Path) -> None:
    """``ctx`` must not leak into the JSON schema of any testreport tool."""
    pytest.importorskip("mcp")
    from mcp.server.fastmcp import FastMCP

    sess = _null_session(tmp_path)
    mcp: FastMCP = FastMCP(name="test-mtui-mcp-schema")
    tt.register_testreport_tools(mcp, sess)

    for name in ("testreport_read", "testreport_patch", "testreport_write"):
        tool = mcp._tool_manager.get_tool(name)  # noqa: SLF001
        assert tool is not None
        props = tool.parameters.get("properties", {})
        assert "ctx" not in props, (
            f"tool {name!r} leaked ctx into its JSON schema: {list(props)!r}"
        )
        assert "ctx" not in tool.parameters.get("required", [])


# --------------------------------------------------------------------------- #
# Per-client isolation (http): two ctx keys -> two templates                  #
# --------------------------------------------------------------------------- #


def test_two_ctx_keys_read_their_own_template(tmp_path: Path) -> None:
    """Distinct request sessions must each ``testreport_read`` only their own file.

    The registered ``testreport_read`` closure resolves the per-call
    session from the provider keyed on ``id(ctx.session)``. We drive it
    with two contexts whose ``.session`` objects differ; a real
    :class:`SessionRegistry` mints one isolated session per key, and we
    seed each with its own loaded testreport path. If the closure
    leaked a shared session, both reads would return the same file.
    """
    pytest.importorskip("mcp")
    from mcp.server.fastmcp import FastMCP

    from mtui.mcp.registry import SessionRegistry

    file_a = tmp_path / "a.txt"
    file_a.write_text("alpha\n", encoding="utf-8")
    file_b = tmp_path / "b.txt"
    file_b.write_text("beta\nbeta2\n", encoding="utf-8")

    # Each minted session is a real McpSession, then re-pointed at its
    # own loaded testreport (NullTestReport -> a SimpleNamespace path).
    seeds = [file_a, file_b]

    def seeding_factory(cfg: object, log: object) -> McpSession:
        session = _null_session(tmp_path)
        session.metadata = SimpleNamespace(path=str(seeds.pop(0)))
        return session

    registry = SessionRegistry(
        seeding_factory,
        _config(tmp_path),
        logging.getLogger("test.mcp.testreport"),
    )
    mcp: FastMCP = FastMCP(name="test-mtui-mcp-isolation")
    tt.register_testreport_tools(mcp, registry)

    read_tool = mcp._tool_manager.get_tool("testreport_read")  # noqa: SLF001
    assert read_tool is not None
    read = read_tool.fn

    # Two distinct request sessions -> two distinct registry keys.
    ctx_a = SimpleNamespace(session=object())
    ctx_b = SimpleNamespace(session=object())

    async def driver() -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
        # Interleave to prove the resolution is per-ctx, not first-wins.
        first = await read(ctx=ctx_a)
        second = await read(ctx=ctx_b)
        third = await read(ctx=ctx_a)  # ctx_a again -> still its own file
        return first, second, third

    first, second, third = asyncio.run(driver())

    assert first["content"] == "alpha\n"
    assert first["path"] == str(file_a)
    assert second["content"] == "beta\nbeta2\n"
    assert second["path"] == str(file_b)
    # Re-reading ctx_a must still hit file_a (cached session, not file_b).
    assert third["content"] == "alpha\n"
    assert third["path"] == str(file_a)
