"""Tests for :mod:`mtui.mcp.profiles` — the selectable tool-surface profiles."""

from __future__ import annotations

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

from mtui.mcp.profiles import (  # noqa: E402
    CORE,
    apply_profile,
    resolve_keep_set,
)
from mtui.mcp.testreport_tools import register_testreport_tools  # noqa: E402
from mtui.mcp.tools import build_tools, register_job_tools  # noqa: E402


class _Provider:
    async def get_or_create(self, key):  # noqa: ANN001, ANN201, D102
        raise AssertionError("not used")


def _build() -> FastMCP:
    mcp = FastMCP(name="mtui-test")
    prov = _Provider()
    build_tools(mcp, prov)
    register_job_tools(mcp, prov)
    register_testreport_tools(mcp, prov)
    return mcp


def test_full_keeps_everything() -> None:
    reg = {"run", "update", "whoami"}
    assert resolve_keep_set(reg, "full") == reg


def test_core_intersects_with_registered() -> None:
    reg = {"run", "whoami", "set_log_level"}
    keep = resolve_keep_set(reg, "core")
    assert "run" in keep  # in CORE
    assert "set_log_level" not in keep  # not in CORE
    assert "whoami" not in keep  # not in CORE


def test_allow_adds_back_only_registered() -> None:
    reg = {"run", "whoami"}
    keep = resolve_keep_set(reg, "core", allow=("whoami", "ghost"))
    assert "whoami" in keep
    assert "ghost" not in keep  # not registered → not invented


def test_deny_wins_last() -> None:
    reg = {"run", "update"}
    keep = resolve_keep_set(reg, "full", deny=("run",))
    assert "run" not in keep
    assert "update" in keep


def test_allow_then_deny_same_name_denies() -> None:
    reg = {"run"}
    keep = resolve_keep_set(reg, "core", allow=("run",), deny=("run",))
    assert "run" not in keep


def test_unknown_profile_falls_back_to_full() -> None:
    reg = {"run", "whoami"}
    assert resolve_keep_set(reg, "does-not-exist") == reg


def test_apply_full_is_noop() -> None:
    mcp = _build()
    before = set(mcp._tool_manager._tools)
    remaining = apply_profile(mcp, "full")
    assert set(remaining) == before
    assert set(mcp._tool_manager._tools) == before


def test_apply_core_removes_non_core_tools() -> None:
    mcp = _build()
    all_names = set(mcp._tool_manager._tools)
    remaining = apply_profile(mcp, "core")
    assert set(remaining) == (CORE & all_names)
    assert set(mcp._tool_manager._tools) == (CORE & all_names)
    # A known non-core tool is gone; a known core tool remains.
    assert "set_log_level" not in remaining
    assert "run" in remaining


def test_apply_core_with_allow_and_deny() -> None:
    mcp = _build()
    remaining = apply_profile(mcp, "core", allow=("whoami",), deny=("run",))
    assert "whoami" in remaining
    assert "run" not in remaining


def test_core_names_all_exist_in_registry() -> None:
    mcp = _build()
    registered = set(mcp._tool_manager._tools)
    # Guards against typos in the curated CORE set.
    assert registered >= CORE
