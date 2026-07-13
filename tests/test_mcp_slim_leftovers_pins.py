"""Mutation-killing pins for two leftover gaps in :mod:`mtui.mcp._slim`.

* ``_slim``'s ``keys_are_names`` flag must propagate correctly through
  ``items`` sub-schemas and ``anyOf`` list members so a nested ``title``
  still gets dropped. ``tests/test_mcp_slim.py`` drives the real
  pydantic-generated schemas for mtui commands, none of which happen to
  carry a ``title`` inside an ``items`` dict or an ``anyOf`` list member,
  so an ``and`` -> ``or`` mutant on the propagation expression (and the
  twin "list items always recurse as non-names" mutant) survived. This
  test hand-builds a schema shaped exactly like that to exercise both
  paths directly.
* ``slim_registered_tools``: the existing
  ``test_slim_registered_tools_reduces_bytes_keeps_count`` only checks
  the aggregate byte count, tool count, and absence of ``title``
  keywords — all of which pass even if every tool's ``parameters`` were
  replaced by ``None`` (``json.dumps(None)`` is 4 bytes, and the
  title-walker no-ops on non-dict/non-list nodes). This test spot-checks
  one specific tool still has a ``dict`` schema with its expected
  parameter names after slimming.
"""

from __future__ import annotations

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

from mtui.mcp._slim import _slim, slim_registered_tools  # noqa: E402
from mtui.mcp.tools import build_tools, register_job_tools  # noqa: E402


class _Provider:
    async def get_or_create(self, key):  # noqa: ANN001, ANN201, D102
        raise AssertionError("not used in schema tests")


# --------------------------------------------------------------------------- #
# _slim: keys_are_names propagates into items/ and anyOf members             #
# --------------------------------------------------------------------------- #


def test_slim_drops_title_nested_in_items_and_anyof_members() -> None:
    """A hand-built schema with ``title`` inside ``items`` and ``anyOf``.

    Also carries an ``enum`` sibling *after* ``description`` inside the
    ``items`` sub-schema so a ``continue`` -> ``break`` mutant on the
    description branch (which would abandon the rest of that dict's
    keys) is caught too: ``enum`` must still survive.
    """
    schema = {
        "properties": {
            "x": {
                "type": "array",
                "items": {
                    "title": "T",
                    "description": "d",
                    "enum": ["a", "b"],
                },
            }
        },
        "anyOf": [
            {"title": "U", "type": "string"},
            {"type": "integer"},
        ],
    }

    result = _slim(schema, keys_are_names=False)

    assert result == {
        "properties": {
            "x": {
                "type": "array",
                "items": {"description": "d", "enum": ["a", "b"]},
            }
        },
        "anyOf": [
            {"type": "string"},
            {"type": "integer"},
        ],
    }

    def _walk_no_title(node: object) -> None:
        if isinstance(node, dict):
            assert "title" not in node
            for value in node.values():
                _walk_no_title(value)
        elif isinstance(node, list):
            for item in node:
                _walk_no_title(item)

    _walk_no_title(result)


# --------------------------------------------------------------------------- #
# slim_registered_tools: schemas stay real dicts, not a wholesale wipe        #
# --------------------------------------------------------------------------- #


def test_slim_registered_tools_keeps_a_real_schema_on_a_known_tool() -> None:
    """After slimming, ``job_status`` still has a dict schema with ``job_id``.

    A mutant that replaces every tool's ``parameters`` with ``None``
    (or slims ``None`` instead of the real schema, yielding the same
    ``None`` result) satisfies the byte-count and no-title checks in
    ``test_slim_registered_tools_reduces_bytes_keeps_count`` but would
    break every MCP client's tool calls. This spot-checks one concrete,
    stable tool's schema shape survives slimming intact.
    """
    mcp = FastMCP(name="mtui-test")
    prov = _Provider()
    build_tools(mcp, prov)
    register_job_tools(mcp, prov)

    tool = mcp._tool_manager.get_tool("job_status")  # noqa: SLF001
    assert tool is not None

    n = slim_registered_tools(mcp)
    assert n > 0

    params = tool.parameters
    assert isinstance(params, dict)
    assert "properties" in params
    assert "job_id" in params["properties"]
    assert params["properties"]["job_id"].get("type") == "string"
