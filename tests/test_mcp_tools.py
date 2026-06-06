"""Tests for :mod:`mtui.mcp.tools` and its argparse \u2192 schema helpers.

The tests stay close to the verify clauses listed in ``PLAN.md`` step 7:

* deny-list commands are absent, expected commands are present,
* exotic argparse shapes round-trip to argv without loss,
* the ``run`` tool's schema matches the contract LLM clients rely on,
* ``config`` subparsers are fanned out to ``config_show`` / ``config_set``,
* read-only commands carry ``readOnlyHint=True``.

We avoid pulling in :class:`fastmcp.Client`: the round-trip behaviour
is exercised in ``tests/test_mcp_stdio_roundtrip.py`` (PLAN step 9).
Here we test the synthesis layer directly, which keeps the suite fast
and decoupled from FastMCP's transport surface.
"""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from typing import TYPE_CHECKING
from unittest.mock import MagicMock

import pytest

pytest.importorskip("fastmcp")

from fastmcp import FastMCP  # noqa: E402

from mtui.commands import Command  # noqa: E402
from mtui.mcp._argv import kwargs_to_argv  # noqa: E402
from mtui.mcp._schema import build_parameters  # noqa: E402
from mtui.mcp.deny import REPL_ONLY  # noqa: E402
from mtui.mcp.session import McpSession  # noqa: E402
from mtui.mcp.tools import (  # noqa: E402
    SUBPARSER_COMMANDS,
    _is_read_only,
    build_tools,
)

if TYPE_CHECKING:
    from collections.abc import Iterable


# --------------------------------------------------------------------------- #
# Fixtures                                                                    #
# --------------------------------------------------------------------------- #


@pytest.fixture
def session(tmp_path: Path) -> McpSession:
    """A real :class:`McpSession` backed by a MagicMock Config."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return McpSession(cfg, logging.getLogger("test.mcp.tools"))


@pytest.fixture
def mcp() -> FastMCP:
    """A fresh FastMCP server with no tools pre-registered."""
    return FastMCP(name="mtui-test")


@pytest.fixture
def registered_names(mcp: FastMCP, session: McpSession) -> list[str]:
    """Tool names registered by :func:`build_tools` against ``mcp``."""
    return build_tools(mcp, session)


def _params_of(mcp: FastMCP, name: str) -> dict:
    """Return the JSON-Schema parameters dict for tool ``name``."""

    async def _get() -> dict:
        tool = await mcp.get_tool(name)
        assert tool is not None, f"tool {name!r} not registered"
        return tool.parameters

    return asyncio.run(_get())


def _annotations_of(mcp: FastMCP, name: str):
    """Return the ``ToolAnnotations`` envelope for tool ``name``."""

    async def _get():
        tool = await mcp.get_tool(name)
        assert tool is not None, f"tool {name!r} not registered"
        return tool.annotations

    return asyncio.run(_get())


# --------------------------------------------------------------------------- #
# Registration coverage                                                       #
# --------------------------------------------------------------------------- #


def test_build_tools_excludes_deny_list(registered_names: list[str]) -> None:
    """Every deny-listed command must be absent from the tool surface."""
    for name in REPL_ONLY:
        assert name not in registered_names, (
            f"deny-listed command {name!r} leaked into the MCP tool surface"
        )


def test_build_tools_excludes_subparser_parent(
    registered_names: list[str],
) -> None:
    """Subparser parents are fanned out; the bare parent must not be a tool."""
    for parent in SUBPARSER_COMMANDS:
        assert parent not in registered_names


def test_build_tools_includes_expected_commands(
    registered_names: list[str],
) -> None:
    """A representative cross-section of commands must be exposed."""
    expected: Iterable[str] = (
        "whoami",
        "run",
        "add_host",
        "assign",
        "update",
        "load_template",
        "list_hosts",
        "openqa_overview",
        "reject",
    )
    missing = [c for c in expected if c not in registered_names]
    assert not missing, f"missing tools: {missing}"


def test_subparser_fan_out_registers_show_and_set(
    registered_names: list[str],
) -> None:
    """``config`` becomes ``config_show`` + ``config_set``."""
    assert "config_show" in registered_names
    assert "config_set" in registered_names


# --------------------------------------------------------------------------- #
# Schema fidelity                                                             #
# --------------------------------------------------------------------------- #


def test_run_tool_schema_has_command_and_hosts_list(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """The ``run`` tool's REMAINDER positional becomes ``array<string>``."""
    params = _params_of(mcp, "run")
    props = params["properties"]
    assert props["command"] == {
        "type": "array",
        "items": {"type": "string"},
        "default": [],
        "description": "Command to run on refhost",
    }
    assert props["hosts"]["type"] == "array"
    assert props["hosts"]["items"] == {"type": "string"}
    # Optional list: default empty, not in required.
    assert "required" not in params or "hosts" not in params["required"]


def test_set_host_state_schema_carries_enum(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``choices=[...]`` on the parser becomes a JSON-Schema ``enum``."""
    params = _params_of(mcp, "set_host_state")
    state = params["properties"]["state"]
    # ``nargs=1`` wraps the choice in an array.
    assert state["type"] == "array"
    assert state["items"]["enum"] == [
        "parallel",
        "serial",
        "dryrun",
        "disabled",
        "enabled",
    ]


def test_update_schema_exposes_store_const_as_booleans(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``store_const`` flags collapse to booleans with a hint in the description."""
    params = _params_of(mcp, "update")["properties"]
    for flag in ("newpackage", "noprepare", "noscript"):
        assert params[flag]["type"] == "boolean"
        assert "sets " in params[flag]["description"]


def test_config_set_schema_requires_attribute_and_value(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """The fanned-out ``config_set`` carries the subparser's required positionals."""
    params = _params_of(mcp, "config_set")
    required = set(params.get("required", []))
    assert {"attribute", "value"}.issubset(required)
    assert params["properties"]["attribute"]["type"] == "string"
    assert params["properties"]["value"]["type"] == "string"


# --------------------------------------------------------------------------- #
# Read-only heuristic                                                         #
# --------------------------------------------------------------------------- #


def test_read_only_heuristic_matches_allow_list() -> None:
    """The internal helper honours the prefix + exact allow-list."""
    assert _is_read_only("whoami")
    assert _is_read_only("list_hosts")
    assert _is_read_only("show_log")
    assert _is_read_only("openqa_overview")
    assert _is_read_only("products")
    assert not _is_read_only("update")
    assert not _is_read_only("approve")
    assert not _is_read_only("config_set")


def test_read_only_annotation_set_for_known_safe_tools(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """The hint must reach the FastMCP tool envelope."""
    for name in ("whoami", "list_hosts", "show_log", "openqa_overview"):
        assert _annotations_of(mcp, name).readOnlyHint is True

    for name in ("update", "approve", "reject"):
        assert _annotations_of(mcp, name).readOnlyHint is False


# --------------------------------------------------------------------------- #
# kwargs \u2192 argv reserialisation round-trips                                #
# --------------------------------------------------------------------------- #


def test_store_true_flag_round_trip() -> None:
    """``store_true`` true \u2192 long flag emitted; false \u2192 omitted."""
    parser = Command.registry["add_host"].argparser(__import__("sys"))
    assert kwargs_to_argv(parser, {"keep_mode": True, "target": []}) == ["--keep-mode"]
    assert kwargs_to_argv(parser, {"keep_mode": False, "target": []}) == []


def test_append_flag_round_trip() -> None:
    """Each list element of an ``append`` arg gets its own flag instance."""
    parser = Command.registry["add_host"].argparser(__import__("sys"))
    argv = kwargs_to_argv(parser, {"target": ["h1", "h2"], "keep_mode": False})
    assert argv == ["--target", "h1", "--target", "h2"]
    parsed = parser.parse_args(argv)
    assert parsed.target == ["h1", "h2"]


def test_remainder_positional_appended_after_flags() -> None:
    """``run`` re-emits REMAINDER positional after flag-shaped args."""
    parser = Command.registry["run"].argparser(__import__("sys"))
    argv = kwargs_to_argv(parser, {"command": ["ls", "-la"], "hosts": ["h1"]})
    assert argv == ["--target", "h1", "ls", "-la"]
    parsed = parser.parse_args(argv)
    assert parsed.command == ["ls", "-la"]
    assert parsed.hosts == ["h1"]


def test_store_const_flag_round_trip() -> None:
    """``update --noscript`` round-trips both ways."""
    parser = Command.registry["update"].argparser(__import__("sys"))
    argv = kwargs_to_argv(
        parser,
        {"noscript": True, "newpackage": False, "noprepare": False, "hosts": []},
    )
    assert argv == ["--noscript"]
    parsed = parser.parse_args(argv)
    assert parsed.noscript == "noscript"
    assert parsed.newpackage is None
    assert parsed.noprepare is None


def test_optional_multivalue_flag_round_trip() -> None:
    """``reject --message ...`` emits the flag once followed by every value.

    Emitting ``--message x --message y`` would be wrong: REMAINDER on
    an optional consumes all remaining tokens, so the second ``-m``
    would be swallowed as a value rather than start a new instance.
    """
    parser = Command.registry["reject"].argparser(__import__("sys"))
    argv = kwargs_to_argv(
        parser,
        {
            "reason": "admin",
            "message": ["why", "not"],
            "group": [],
            "user": "",
        },
    )
    parsed = parser.parse_args(argv)
    assert parsed.reason == "admin"
    assert parsed.message == ["why", "not"]


def test_optional_list_arg_preserves_nonempty_argparse_default() -> None:
    """``openqa_overview --aggregated-groups`` defaults to ``["core"]``.

    Regression for the MCP-side bug where an optional ``nargs="+"`` arg
    with a non-empty argparse default was rewritten to ``[]``, causing
    ``kwargs_to_argv`` to emit a bare ``--aggregated-groups`` flag and
    argparse to fail with *"expected at least one argument"*.
    """
    parser = Command.registry["openqa_overview"].argparser(__import__("sys"))
    params = {p.name: p for p in build_parameters(parser)}
    assert "aggregated_groups" in params
    assert params["aggregated_groups"].default == ["core"]

    # Round-trip the defaulted value through the encoder + argparse.
    argv = kwargs_to_argv(parser, {"aggregated_groups": ["core"]})
    assert argv == ["--aggregated-groups", "core"]
    parsed = parser.parse_args(argv)
    assert parsed.aggregated_groups == ["core"]


def test_required_choice_appears_in_schema_required(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``reject``'s ``-r/--reason`` is ``required=True`` \u2192 schema ``required``."""
    params = _params_of(mcp, "reject")
    assert "reason" in params.get("required", [])
    assert params["properties"]["reason"]["enum"] == [
        "admin",
        "retracted",
        "build_problem",
        "not_fixed",
        "regression",
        "false_reject",
        "tracking_issue",
    ]


# --------------------------------------------------------------------------- #
# Schema synthesis pure-function checks                                       #
# --------------------------------------------------------------------------- #


def test_build_parameters_drops_help_action() -> None:
    """``-h/--help`` must never become a tool parameter."""
    parser = Command.registry["whoami"].argparser(__import__("sys"))
    params = build_parameters(parser)
    assert all(p.name != "help" for p in params)


def test_build_parameters_loadtemplate_exposes_both_review_ids(caplog) -> None:
    """``load_template`` mutex StoreAction group: each long flag gets its own param.

    Regression for the original ``duplicate dest 'update' in parser
    'load_template'`` boot warning that hid the kernel review-id path
    from MCP clients.
    """
    parser = Command.registry["load_template"].argparser(__import__("sys"))
    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        params = build_parameters(parser)
    names = [p.name for p in params]
    assert "auto_review_id" in names
    assert "kernel_review_id" in names
    assert "update" not in names
    # No more "duplicate dest" warning for either side of the group.
    assert not any("duplicate dest" in r.message for r in caplog.records)


def test_build_parameters_setrepo_collapses_to_operation_enum(caplog) -> None:
    """``set_repo`` mutex StoreConst group becomes a single Literal enum.

    Regression for the original ``duplicate dest 'operation' in parser
    'set_repo'`` boot warning that hid the ``remove`` operation from
    MCP clients.
    """
    parser = Command.registry["set_repo"].argparser(__import__("sys"))
    with caplog.at_level(logging.WARNING, logger="mtui.mcp.schema"):
        params = build_parameters(parser)
    names = [p.name for p in params]
    assert names.count("operation") == 1
    assert not any("duplicate dest" in r.message for r in caplog.records)


def test_build_parameters_required_before_optional() -> None:
    """Required parameters must precede optionals (``inspect.Signature`` rule)."""
    parser = Command.registry["reject"].argparser(__import__("sys"))
    params = build_parameters(parser)
    seen_optional = False
    for p in params:
        if p.default is not p.empty:
            seen_optional = True
        else:
            assert not seen_optional, (
                f"required parameter {p.name!r} after an optional one"
            )


# --------------------------------------------------------------------------- #
# Mutex-group MCP surface regressions                                         #
# --------------------------------------------------------------------------- #


def test_setrepo_operation_enum_in_schema(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``set_repo`` exposes ``operation`` as a required ``add``/``remove`` enum."""
    params = _params_of(mcp, "set_repo")
    assert "operation" in params.get("required", [])
    enum_values = params["properties"]["operation"]["enum"]
    assert sorted(enum_values) == ["add", "remove"]


def test_setrepo_operation_remove_routes_to_remove_flag() -> None:
    """``operation='remove'`` must emit ``--remove``, not ``--add``."""
    parser = Command.registry["set_repo"].argparser(__import__("sys"))
    # Prime the parser's synthetic-name metadata (build_parameters
    # attaches it; _argv reads it).
    build_parameters(parser)
    argv = kwargs_to_argv(parser, {"operation": "remove", "hosts": []})
    assert "--remove" in argv
    assert "--add" not in argv


def test_setrepo_operation_add_routes_to_add_flag() -> None:
    """``operation='add'`` must emit ``--add``, not ``--remove``."""
    parser = Command.registry["set_repo"].argparser(__import__("sys"))
    build_parameters(parser)
    argv = kwargs_to_argv(parser, {"operation": "add", "hosts": []})
    assert "--add" in argv
    assert "--remove" not in argv


def test_loadtemplate_kernel_id_routes_to_kernel_flag() -> None:
    """``kernel_review_id`` must emit ``--kernel-review-id``, not ``--auto-review-id``."""
    parser = Command.registry["load_template"].argparser(__import__("sys"))
    build_parameters(parser)
    argv = kwargs_to_argv(
        parser,
        {
            "kernel_review_id": "SUSE:Maintenance:1:1",
            "auto_review_id": None,
            "chosts": True,
        },
    )
    assert argv[:2] == ["--kernel-review-id", "SUSE:Maintenance:1:1"]
    assert "--auto-review-id" not in argv


def test_loadtemplate_auto_id_routes_to_auto_flag() -> None:
    """``auto_review_id`` must emit ``--auto-review-id``, not ``--kernel-review-id``."""
    parser = Command.registry["load_template"].argparser(__import__("sys"))
    build_parameters(parser)
    argv = kwargs_to_argv(
        parser,
        {
            "auto_review_id": "SUSE:Maintenance:1:1",
            "kernel_review_id": None,
            "chosts": True,
        },
    )
    assert argv[:2] == ["--auto-review-id", "SUSE:Maintenance:1:1"]
    assert "--kernel-review-id" not in argv


def test_loadtemplate_schema_exposes_both_review_ids(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``load_template`` schema must show both review-id params, neither in ``required``."""
    params = _params_of(mcp, "load_template")
    props = params["properties"]
    assert "auto_review_id" in props
    assert "kernel_review_id" in props
    # Both are individually optional in the JSON schema (the
    # "exactly one" rule is enforced at call time by the wrapper).
    required = params.get("required", [])
    assert "auto_review_id" not in required
    assert "kernel_review_id" not in required
    assert "update" not in props


def test_loadtemplate_rejects_neither_review_id(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Calling ``load_template`` with no review id must surface a clean error."""
    from mtui.mcp.session import McpCommandError

    async def _call() -> str:
        tool = await mcp.get_tool("load_template")
        assert tool is not None
        return await tool.fn(  # ty: ignore[unresolved-attribute]
            auto_review_id=None,
            kernel_review_id=None,
            chosts=True,
        )

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(_call())
    assert ei.value.exit_code == 2
    assert "exactly one" in ei.value.stderr


def test_loadtemplate_rejects_both_review_ids(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Calling ``load_template`` with both review ids must surface a clean error."""
    from mtui.mcp.session import McpCommandError

    async def _call() -> str:
        tool = await mcp.get_tool("load_template")
        assert tool is not None
        return await tool.fn(  # ty: ignore[unresolved-attribute]
            auto_review_id="SUSE:Maintenance:1:1",
            kernel_review_id="SUSE:Maintenance:2:2",
            chosts=True,
        )

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(_call())
    assert ei.value.exit_code == 2
    assert "exactly one" in ei.value.stderr
