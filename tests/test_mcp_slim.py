"""Tests for :mod:`mtui.mcp._slim` — the tool-schema token-slimming pass.

Covers the three slimming transforms (drop ``title``, flatten ``[T, null]``
unions, terse shared descriptions), the in-place rewrite over a live FastMCP
tool table, and the :func:`cap_output` budget helper.
"""

from __future__ import annotations

import json

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

from mtui.mcp._slim import (  # noqa: E402
    cap_output,
    slim_registered_tools,
    slim_tool_schema,
)
from mtui.mcp.tools import build_tools, register_job_tools  # noqa: E402


class _Provider:
    async def get_or_create(self, key):  # noqa: ANN001, ANN201, D102
        raise AssertionError("not used in schema tests")


# --------------------------------------------------------------------------- #
# slim_tool_schema                                                            #
# --------------------------------------------------------------------------- #


def test_slim_drops_all_title_keys() -> None:
    schema = {
        "title": "tool_runArguments",
        "type": "object",
        "properties": {
            "command": {"title": "Command", "type": "array", "items": {"type": "str"}},
        },
    }
    out = slim_tool_schema(schema)
    assert "title" not in out
    assert "title" not in out["properties"]["command"]
    # non-title content preserved
    assert out["properties"]["command"]["type"] == "array"


def test_slim_flattens_nullable_union_and_keeps_default() -> None:
    node = {
        "anyOf": [{"type": "string"}, {"type": "null"}],
        "default": None,
        "description": "some field",
    }
    out = slim_tool_schema(node)
    assert "anyOf" not in out
    assert out["type"] == "string"
    assert out["default"] is None
    assert out["description"] == "some field"


def test_slim_flattens_nullable_array_union_hoists_items() -> None:
    node = {
        "anyOf": [{"type": "array", "items": {"type": "string"}}, {"type": "null"}],
        "default": None,
    }
    out = slim_tool_schema(node)
    assert "anyOf" not in out
    assert out["type"] == "array"
    assert out["items"] == {"type": "string"}


def test_slim_leaves_genuine_multitype_union_untouched() -> None:
    node = {"anyOf": [{"type": "string"}, {"type": "integer"}]}
    out = slim_tool_schema(node)
    # No null arm → not the [T, null] shape → left as-is.
    assert "anyOf" in out
    assert len(out["anyOf"]) == 2


def test_slim_rewrites_known_verbose_description() -> None:
    node = {
        "type": "string",
        "default": None,
        "description": (
            "RRID of a single loaded template to act on (default: all loaded templates)"
        ),
    }
    out = slim_tool_schema(node)
    assert out["description"] == "RRID of one loaded template (default: all)"


def test_slim_input_not_mutated() -> None:
    schema = {"title": "X", "properties": {"a": {"title": "A", "type": "string"}}}
    before = json.dumps(schema, sort_keys=True)
    slim_tool_schema(schema)
    assert json.dumps(schema, sort_keys=True) == before


# --------------------------------------------------------------------------- #
# slim_registered_tools                                                       #
# --------------------------------------------------------------------------- #


def test_slim_registered_tools_reduces_bytes_keeps_count() -> None:
    mcp = FastMCP(name="mtui-test")
    prov = _Provider()
    build_tools(mcp, prov)
    register_job_tools(mcp, prov)

    tools = mcp._tool_manager._tools
    before_count = len(tools)
    before_bytes = sum(len(json.dumps(t.parameters)) for t in tools.values())

    n = slim_registered_tools(mcp)

    after_bytes = sum(len(json.dumps(t.parameters)) for t in tools.values())
    assert n == before_count
    assert len(tools) == before_count  # no tool lost
    # At least a 15% schema shrink (titles alone are ~16%).
    assert after_bytes < before_bytes * 0.85
    # No residual boilerplate anywhere.
    blob = " ".join(json.dumps(t.parameters) for t in tools.values())
    assert '"title"' not in blob
    assert '{"type": "null"}' not in blob


def test_slim_registered_tools_missing_table_is_graceful() -> None:
    class Fake:
        pass

    # No _tool_manager attribute at all → returns 0, no raise.
    assert slim_registered_tools(Fake()) == 0  # ty: ignore[invalid-argument-type]


# --------------------------------------------------------------------------- #
# cap_output                                                                  #
# --------------------------------------------------------------------------- #


def test_cap_output_under_limit_is_identical() -> None:
    text = "small output"
    assert cap_output(text, 1000) == text


def test_cap_output_zero_limit_disables() -> None:
    text = "x" * 10_000
    assert cap_output(text, 0) == text
    assert cap_output(text, -1) == text


def test_cap_output_truncates_with_notice() -> None:
    text = "A" * 500
    out = cap_output(text, 100)
    assert out.startswith("A" * 100)
    assert "truncated 400 bytes" in out
    assert "max_output_bytes=100" in out


def test_cap_output_result_is_valid_utf8_on_codepoint_boundary() -> None:
    # Multi-byte chars; a naive byte cut could split one. Result must decode.
    text = "é" * 200  # each is 2 bytes in utf-8
    out = cap_output(text, 101)  # odd cut lands mid-codepoint
    out.encode("utf-8")  # must not raise
    assert "truncated" in out
