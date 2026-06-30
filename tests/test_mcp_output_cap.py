"""Tests for the Tier-3 output-cap wired into the testreport read tools.

:func:`mtui.mcp._slim.cap_output` itself is unit-tested in
``tests/test_mcp_slim.py``; here we assert the cap is actually applied to the
``content`` returned by ``testreport_read`` / ``testreport_read_file`` using the
session's ``config.mcp_max_output_bytes``.
"""

from __future__ import annotations

import asyncio
import contextlib
from pathlib import Path
from types import SimpleNamespace

import pytest

pytest.importorskip("mcp")

from mtui.mcp import testreport_tools as tt  # noqa: E402


@contextlib.asynccontextmanager
async def _noop_lock(_template=None):  # noqa: ANN001, ANN202
    yield


def _session(path: Path, cap: int) -> SimpleNamespace:
    """Fake session exposing only what the testreport read tools touch."""
    metadata = SimpleNamespace(id="SUSE:Maintenance:1:1", path=str(path))

    class _Reg:
        def get(self, rrid):  # noqa: ANN001, ANN202
            return metadata

        def all(self):  # noqa: ANN202
            return [metadata]

    return SimpleNamespace(
        metadata=metadata,
        templates=_Reg(),
        scoped_lock=lambda _t=None: _noop_lock(),
        config=SimpleNamespace(mcp_max_output_bytes=cap),
    )


def test_read_caps_oversized_content(tmp_path: Path) -> None:
    log = tmp_path / "log"
    log.write_text("A" * 5000, encoding="utf-8")
    sess = _session(log, cap=100)

    res = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]
    assert res["content"].startswith("A" * 100)
    assert "truncated" in res["content"]
    # line_count reflects the true file, not the truncated view.
    assert res["line_count"] == 1


def test_read_under_cap_is_identical(tmp_path: Path) -> None:
    log = tmp_path / "log"
    body = "hello\nworld\n"
    log.write_text(body, encoding="utf-8")
    sess = _session(log, cap=100_000)

    res = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]
    assert res["content"] == body
    assert "truncated" not in res["content"]


def test_read_cap_zero_disables(tmp_path: Path) -> None:
    log = tmp_path / "log"
    body = "B" * 5000
    log.write_text(body, encoding="utf-8")
    sess = _session(log, cap=0)

    res = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]
    assert res["content"] == body


def test_read_file_caps_oversized_content(tmp_path: Path) -> None:
    log = tmp_path / "log"
    log.write_text("x", encoding="utf-8")
    big = tmp_path / "build_checks"
    big.mkdir()
    (big / "pkg.x86_64.log").write_text("C" * 5000, encoding="utf-8")
    sess = _session(log, cap=100)

    call = tt.testreport_read_file(sess, "build_checks/pkg.x86_64.log")  # ty: ignore[invalid-argument-type]
    res = asyncio.run(call)
    assert res["content"].startswith("C" * 100)
    assert "truncated" in res["content"]
