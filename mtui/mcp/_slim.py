"""Token-slimming pass over the synthesised MCP tool JSON schemas.

The tools built by :mod:`mtui.mcp.tools` (one per :class:`mtui.commands.Command`)
get their input schema from :mod:`pydantic` via the SDK. Pydantic's generator is
faithful but verbose: every field carries a redundant ``"title"`` key, every
optional field is rendered as a two-member ``anyOf: [{type: X}, {type: null}]``
union, and the shared ``template`` / ``all_templates`` / ``hosts`` help strings
are repeated across dozens of tools. None of that changes what the model can
call, but all of it is sent on **every** request as part of the tool list.

This module rewrites the already-registered schemas in place to drop the dead
weight while preserving meaning (type, default, description, enum). It is a pure
transform on plain ``dict`` data plus a thin walker over the SDK's tool table;
both are isolated here so an SDK rename only breaks one well-tested seam (see
:func:`slim_registered_tools`, which degrades gracefully).
"""

from __future__ import annotations

from logging import getLogger
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from mcp.server.fastmcp import FastMCP

logger = getLogger("mtui.mcp.slim")


def cap_output(text: str, limit: int) -> str:
    """Truncate ``text`` to at most ``limit`` bytes (utf-8), with a notice.

    A single tool result — a ``run`` over many hosts, a multi-thousand-line
    install log — can dwarf the rest of the context. When the utf-8 encoding of
    ``text`` exceeds ``limit`` the **tail** is dropped (the head usually carries
    the command echo and the first, most diagnostic output) and a one-line
    ``…[truncated N bytes; …]`` notice is appended pointing at the paged readers.

    ``limit <= 0`` (or a non-integer ``limit``) disables the cap and returns
    ``text`` unchanged. Under-cap text is returned byte-identical. The cut is
    made on a decoded boundary so the result is always valid utf-8 even if the
    byte cut would split a codepoint.
    """
    if not isinstance(limit, int) or isinstance(limit, bool) or limit <= 0:
        return text
    encoded = text.encode("utf-8")
    if len(encoded) <= limit:
        return text
    dropped = len(encoded) - limit
    head = encoded[:limit].decode("utf-8", errors="ignore")
    notice = (
        f"\n…[truncated {dropped} bytes; output exceeded the "
        f"[mcp] max_output_bytes={limit} budget — use a narrower command, or "
        f"the offset/limit paging on testreport reads]"
    )
    return head + notice


#: Long argparse ``help`` strings shared across many synthesised tools, mapped to
#: a terse equivalent. Rewriting only the MCP wire copy keeps the REPL ``--help``
#: output (sourced from the same argparse actions in
#: :mod:`mtui.commands._command`) verbose and unchanged. Keys are matched exactly
#: against a field's ``description``.
_TERSE_DESCRIPTIONS: dict[str, str] = {
    "RRID of a single loaded template to act on (default: all loaded templates)": (
        "RRID of one loaded template (default: all)"
    ),
    "Act on every loaded template (the default for this command)": (
        "Act on all loaded templates (default)"
    ),
    "Host to act on. Can be used multiple times. If is ommited all hosts are used": (
        "Host to act on (repeatable; default: all hosts)"
    ),
}


def _flatten_nullable(node: dict[str, Any]) -> None:
    """Collapse a two-member ``anyOf: [{type: X}, {type: null}]`` union in place.

    Pydantic renders ``X | None`` as such a union. The ``"null"`` arm is
    redundant for the model — the presence of a ``default`` already signals the
    field is optional — so we hoist the non-null arm's ``"type"`` (and any
    sibling keys it carries, e.g. ``items`` for arrays) to the node level and
    drop the ``anyOf``. Only the exact two-member ``[T, null]`` shape is touched;
    anything else (genuine multi-type unions) is left alone.
    """
    any_of = node.get("anyOf")
    if not isinstance(any_of, list) or len(any_of) != 2:
        return
    non_null = [m for m in any_of if isinstance(m, dict) and m.get("type") != "null"]
    nulls = [m for m in any_of if isinstance(m, dict) and m.get("type") == "null"]
    if len(non_null) != 1 or len(nulls) != 1:
        return
    arm = non_null[0]
    if "type" not in arm:
        return
    del node["anyOf"]
    # Hoist the surviving arm's keys without clobbering node-level metadata
    # (description/default/title live on the node, not the arm).
    for key, value in arm.items():
        node.setdefault(key, value)


def slim_tool_schema(schema: Any) -> Any:
    """Return ``schema`` recursively slimmed of redundant JSON-Schema weight.

    Three transforms, applied depth-first so nested ``properties`` and ``items``
    are covered:

    * drop every ``"title"`` key (pydantic emits one per field; the model never
      needs it);
    * collapse ``anyOf: [{type: X}, {type: null}]`` to a flat ``{type: X}`` via
      :func:`_flatten_nullable`;
    * replace a known-verbose ``description`` with its terse form from
      :data:`_TERSE_DESCRIPTIONS`.

    The input is not mutated; a new structure is returned.

    The keys of a ``properties`` / ``$defs``-style map are *names*, not
    schema keywords, so the transforms are suspended for that one level:
    a tool parameter (or nested object property) literally named
    ``title`` or ``description`` must survive -- dropping it left the
    name dangling in ``required`` while its schema vanished.
    """
    return _slim(schema, keys_are_names=False)


#: Schema keywords whose value is a mapping of *names* to sub-schemas.
_NAME_MAPS = frozenset({"properties", "patternProperties", "$defs", "definitions"})


def _slim(schema: Any, *, keys_are_names: bool) -> Any:
    if isinstance(schema, dict):
        out: dict[str, Any] = {}
        for key, value in schema.items():
            if not keys_are_names:
                if key == "title":
                    continue
                if key == "description" and isinstance(value, str):
                    out[key] = _TERSE_DESCRIPTIONS.get(value, value)
                    continue
            out[key] = _slim(
                value,
                keys_are_names=(not keys_are_names and key in _NAME_MAPS),
            )
        if not keys_are_names:
            _flatten_nullable(out)
        return out
    if isinstance(schema, list):
        return [_slim(item, keys_are_names=False) for item in schema]
    return schema


def slim_registered_tools(mcp: FastMCP) -> int:
    """Rewrite every registered tool's input schema in place, slimmed.

    Walks the SDK's tool table (``FastMCP._tool_manager._tools``) and replaces
    each tool's ``parameters`` (the JSON input schema) with
    :func:`slim_tool_schema` of itself. Returns the number of tools rewritten.

    The private-attribute access is the one place this implementation reaches
    into SDK internals; if a future SDK renames the table the walk is skipped
    with a loud warning and the (un-slimmed but fully functional) tools are left
    intact — slimming is a token optimisation, never a correctness requirement.
    """
    try:
        tools = mcp._tool_manager._tools  # noqa: SLF001
    except AttributeError:  # pragma: no cover - SDK shape drift guard
        logger.warning(
            "MCP SDK tool table not found (FastMCP._tool_manager._tools); "
            "skipping schema slimming — tools remain functional but verbose"
        )
        return 0

    count = 0
    for tool in tools.values():
        params = getattr(tool, "parameters", None)
        if isinstance(params, dict):
            tool.parameters = slim_tool_schema(params)
            count += 1
    logger.info("slimmed %d MCP tool schemas", count)
    return count
