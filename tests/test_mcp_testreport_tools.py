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


def test_register_testreport_tools_exposes_three_tools(tmp_path: Path) -> None:
    """Smoke-test: three tools registered, ``readOnlyHint`` set as expected."""
    pytest.importorskip("fastmcp")
    from fastmcp import FastMCP

    sess = _null_session(tmp_path)
    mcp: FastMCP = FastMCP(name="test-mtui-mcp")

    names = tt.register_testreport_tools(mcp, sess)

    assert names == [
        "testreport_patch",
        "testreport_read",
        "testreport_write",
    ]

    async def _fetch() -> dict[str, Any]:
        out: dict[str, Any] = {}
        for n in names:
            tool = await mcp.get_tool(n)
            assert tool is not None, f"tool {n!r} missing after registration"
            out[n] = tool
        return out

    tools = asyncio.run(_fetch())

    read_tool = tools["testreport_read"]
    patch_tool = tools["testreport_patch"]
    write_tool = tools["testreport_write"]

    assert read_tool.annotations is not None
    assert read_tool.annotations.readOnlyHint is True
    assert read_tool.annotations.idempotentHint is True

    # Patch/write must NOT be marked read-only.
    for tool in (patch_tool, write_tool):
        if tool.annotations is not None:
            assert tool.annotations.readOnlyHint is not True

    # The read-first warning is glued onto each description.
    for tool in (read_tool, patch_tool, write_tool):
        assert "testreport_read" in tool.description
