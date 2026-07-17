"""Tests for :mod:`mtui.mcp.tools` and its argparse \u2192 schema helpers.

The tests stay close to the verify clauses listed in ``PLAN.md`` step 7:

* deny-list commands are absent, expected commands are present,
* exotic argparse shapes round-trip to argv without loss,
* the ``run`` tool's schema matches the contract LLM clients rely on,
* ``config`` subparsers are fanned out to ``config_show`` / ``config_set``,
* read-only commands carry ``readOnlyHint=True``.

We avoid pulling in the in-memory MCP client: the round-trip
behaviour is exercised in ``tests/test_mcp_stdio_roundtrip.py``
(PLAN step 9). Here we test the synthesis layer directly, which
keeps the suite fast and decoupled from the MCP server's transport
surface.
"""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from typing import TYPE_CHECKING
from unittest.mock import MagicMock

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

from mtui.commands import Command  # noqa: E402
from mtui.mcp._argv import kwargs_to_argv  # noqa: E402
from mtui.mcp._schema import build_parameters  # noqa: E402
from mtui.mcp.deny import REPL_ONLY  # noqa: E402
from mtui.mcp.session import McpSession  # noqa: E402
from mtui.mcp.testreport_tools import register_testreport_tools  # noqa: E402
from mtui.mcp.tools import (  # noqa: E402
    SLOW_COMMANDS,
    SUBPARSER_COMMANDS,
    _is_read_only,
    build_tools,
    register_job_tools,
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
    """Return the JSON-Schema parameters dict for tool ``name``.

    The SDK's :class:`FastMCP` does not expose a public ``get_tool``;
    we reach through ``_tool_manager`` (synchronous, returns the
    :class:`mcp.server.fastmcp.tools.base.Tool` pydantic model).
    """
    tool = mcp._tool_manager.get_tool(name)  # noqa: SLF001
    assert tool is not None, f"tool {name!r} not registered"
    return tool.parameters


def _annotations_of(mcp: FastMCP, name: str):
    """Return the ``ToolAnnotations`` envelope for tool ``name``."""
    tool = mcp._tool_manager.get_tool(name)  # noqa: SLF001
    assert tool is not None, f"tool {name!r} not registered"
    return tool.annotations


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


def test_registered_tools_advertise_no_output_schema(
    mcp: FastMCP, session: McpSession
) -> None:
    """No registered tool may carry an auto-generated ``outputSchema``.

    The command wrappers and job tools return a plain ``str``; left to the
    SDK default each would advertise an information-free ``{"result": str}``
    output schema -- inflating the manifest the client re-reads each
    session -- and echo the whole text back as ``structuredContent`` on
    every call. The testreport tools return dicts but likewise opt out, to
    keep the wire uniform. Every one of the server's six ``add_tool`` sites
    therefore passes ``structured_output=False``; registering all of them
    here pins that contract so a new site that forgets the flag is caught.
    """
    build_tools(mcp, session)
    register_job_tools(mcp, session)
    register_testreport_tools(mcp, session)

    offenders = [
        name
        for name, tool in mcp._tool_manager._tools.items()  # noqa: SLF001
        if tool.output_schema is not None
    ]
    assert not offenders, f"tools advertising a boilerplate outputSchema: {offenders}"


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
        "unload",
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
    """The ``run`` tool's REMAINDER positional becomes ``array<string>``.

    Uses subset-assertions rather than dict equality because the
    pydantic version bundled with :mod:`mcp.server.fastmcp` adds a
    schema-generated ``title`` key alongside the documented fields.
    """
    params = _params_of(mcp, "run")
    props = params["properties"]
    expected_command = {
        "type": "array",
        "items": {"type": "string"},
        "default": [],
        "description": "Command to run on refhost",
    }
    assert expected_command.items() <= props["command"].items()
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
    # ``nargs=1`` is exposed as a scalar enum (not an array) so MCP
    # clients send ``"parallel"`` instead of ``["parallel"]``.
    assert "type" not in state or state["type"] != "array"
    assert state["enum"] == [
        "parallel",
        "serial",
        "dryrun",
        "disabled",
        "enabled",
    ]


def test_nargs_one_positional_schema_is_scalar_string(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """A ``nargs=1`` positional (``put filename``) is a required scalar string.

    Regression for the MCP-side bug where ``{"filename": "x"}`` was
    rejected with ``Input should be a valid list`` because the schema
    demanded an array.
    """
    params = _params_of(mcp, "put")
    filename = params["properties"]["filename"]
    assert filename["type"] == "string"
    assert filename.get("description") == "file to upload to all hosts"
    # Scalar field must not carry array-shaped length bounds.
    assert "minItems" not in filename
    assert "maxItems" not in filename
    assert "filename" in params.get("required", [])


def test_update_schema_exposes_store_const_as_booleans(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``store_const`` flags collapse to booleans with a hint in the description."""
    params = _params_of(mcp, "update")["properties"]
    for flag in ("newpackage", "noprepare"):
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


def test_synthesised_wrapper_carries_context_parameter() -> None:
    """The synthesised wrapper must expose a ``ctx: Context`` parameter.

    FastMCP's :func:`find_context_parameter` reads
    :func:`typing.get_type_hints` to locate the parameter to inject
    the per-request :class:`Context` into; we therefore need both the
    synthesised :class:`inspect.Signature` AND the ``__annotations__``
    map to advertise ``ctx`` with the real ``Context`` annotation.
    """
    import inspect

    from mcp.server.fastmcp import Context

    from mtui.mcp.tools import _make_wrapper

    session_stub = MagicMock()
    parser = Command.registry["whoami"].argparser(__import__("sys"))
    wrapper = _make_wrapper(Command.registry["whoami"], parser, session_stub)

    sig = inspect.signature(wrapper)
    assert "ctx" in sig.parameters
    ctx_param = sig.parameters["ctx"]
    assert ctx_param.kind is inspect.Parameter.KEYWORD_ONLY
    assert ctx_param.default is None
    assert ctx_param.annotation is Context
    # Annotations map must also resolve to the real class (not a
    # string) so ``typing.get_type_hints`` succeeds without a
    # ForwardRef lookup.
    assert wrapper.__annotations__["ctx"] is Context


def test_tool_schema_does_not_advertise_ctx(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``ctx`` must be stripped from every tool's JSON schema.

    FastMCP's tool registration calls ``func_metadata(fn,
    skip_names=[context_kwarg])`` which excludes the Context-typed
    parameter from the pydantic model used to build the JSON schema.
    Asserted for a representative cross-section so a regression in
    the SDK contract (e.g. a future version that no longer skips
    Context params) surfaces loudly.
    """
    for name in (
        "whoami",
        "run",
        "update",
        "load_template",
        "config_set",
        "config_show",
    ):
        params = _params_of(mcp, name)
        props = params.get("properties", {})
        assert "ctx" not in props, (
            f"tool {name!r} leaked a Context parameter into its JSON schema: "
            f"{list(props)!r}"
        )
        required = params.get("required", [])
        assert "ctx" not in required


def test_read_only_heuristic_matches_allow_list() -> None:
    """The internal helper honours the prefix + exact allow-list."""
    assert _is_read_only("whoami")
    assert _is_read_only("list_hosts")
    assert _is_read_only("list_products")  # covered by the list_ prefix
    assert _is_read_only("show_log")
    assert _is_read_only("openqa_overview")
    assert _is_read_only("openqa_jobs")  # exact: only queries openQA
    assert not _is_read_only("update")
    assert not _is_read_only("approve")
    assert not _is_read_only("config_set")
    # reload_products re-reads products from the hosts -> NOT read-only.
    assert not _is_read_only("reload_products")


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


def test_append_remainder_flag_round_trip_commit() -> None:
    """``commit -m`` (append + REMAINDER) re-emits the flag once, not per token.

    Regression: the append encoder used to emit ``-m a -m b``; with REMAINDER
    the second ``-m`` is swallowed as a value, so the command's
    ``" ".join(self.args.msg[0])`` produced ``"a -m b"`` instead of ``"a b"``.
    """
    parser = Command.registry["commit"].argparser(__import__("sys"))
    argv = kwargs_to_argv(parser, {"msg": ["a", "b"]})
    assert argv == ["--msg", "a", "b"]
    parsed = parser.parse_args(argv)
    assert parsed.msg == [["a", "b"]]
    assert " ".join(parsed.msg[0]) == "a b"


def test_append_remainder_flag_round_trip_lock_with_target() -> None:
    """``lock -t h1 -c ...`` keeps the REMAINDER comment last and intact."""
    parser = Command.registry["lock"].argparser(__import__("sys"))
    argv = kwargs_to_argv(parser, {"hosts": ["h1"], "comment": ["busy", "testing"]})
    # target must precede the REMAINDER comment so it isn't swallowed.
    assert argv == ["--target", "h1", "--comment", "busy", "testing"]
    parsed = parser.parse_args(argv)
    assert parsed.hosts == ["h1"]
    assert " ".join(parsed.comment[0]) == "busy testing"


def test_store_const_flag_round_trip() -> None:
    """``update --noprepare`` round-trips both ways."""
    parser = Command.registry["update"].argparser(__import__("sys"))
    argv = kwargs_to_argv(
        parser,
        {"noprepare": True, "newpackage": False, "hosts": []},
    )
    assert argv == ["--noprepare"]
    parsed = parser.parse_args(argv)
    assert parsed.noprepare == "noprepare"
    assert parsed.newpackage is None


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


def test_remainder_optional_emitted_after_other_flags() -> None:
    """A REMAINDER optional must land after every other flag, not swallow it.

    Regression: when a ``nargs=REMAINDER`` *optional* is declared before
    another flag, emitting it among the flags lets REMAINDER consume the
    later flag as a value (``--message a b --xflag`` -> ``message=['a','b',
    '--xflag']``, ``xflag`` lost). The encoder defers the REMAINDER optional
    to the positional tail so it is always emitted last regardless of
    ``parser._actions`` declaration order.
    """
    import argparse
    from argparse import REMAINDER

    parser = argparse.ArgumentParser(prog="probe")
    parser.add_argument("-m", "--message", nargs=REMAINDER)
    parser.add_argument("-x", "--xflag", action="store_true")

    argv = kwargs_to_argv(parser, {"message": ["a", "b"], "xflag": True})
    # The store_true flag must precede the REMAINDER tokens.
    assert argv == ["--xflag", "--message", "a", "b"]
    parsed = parser.parse_args(argv)
    assert parsed.message == ["a", "b"]
    assert parsed.xflag is True


def test_nargs_one_positional_accepts_scalar_round_trip() -> None:
    """``put filename`` (``nargs=1``) round-trips a scalar string.

    Regression for the MCP-side bug where ``{"filename": "x"}`` was
    rejected with ``Input should be a valid list`` because the schema
    demanded an array. The encoder must accept the scalar and argparse
    must still produce its conventional 1-element list.
    """
    parser = Command.registry["put"].argparser(__import__("sys"))
    argv = kwargs_to_argv(parser, {"filename": "payload.bin"})
    assert argv == ["payload.bin"]
    parsed = parser.parse_args(argv)
    assert parsed.filename == ["payload.bin"]


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

    tool = mcp._tool_manager.get_tool("load_template")  # noqa: SLF001
    assert tool is not None

    async def _call() -> str:
        return await tool.fn(
            auto_review_id=None,
            kernel_review_id=None,
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

    tool = mcp._tool_manager.get_tool("load_template")  # noqa: SLF001
    assert tool is not None

    async def _call() -> str:
        return await tool.fn(
            auto_review_id="SUSE:Maintenance:1:1",
            kernel_review_id="SUSE:Maintenance:2:2",
        )

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(_call())
    assert ei.value.exit_code == 2
    assert "exactly one" in ei.value.stderr


# --------------------------------------------------------------------------- #
# Background-job tool layer                                                    #
# --------------------------------------------------------------------------- #


@pytest.fixture
def job_tool_names(mcp: FastMCP, session: McpSession) -> list[str]:
    """Tool names registered by :func:`register_job_tools` against ``mcp``."""
    return register_job_tools(mcp, session)


def test_register_job_tools_registers_four_tools(
    job_tool_names: list[str],
) -> None:
    """The four background-job control tools must be registered."""
    assert sorted(job_tool_names) == [
        "job_cancel",
        "job_list",
        "job_result",
        "job_status",
    ]


def test_job_read_tools_are_read_only(mcp: FastMCP, job_tool_names: list[str]) -> None:
    """``job_list`` / ``job_status`` / ``job_result`` are side-effect-free."""
    for name in ("job_list", "job_status", "job_result"):
        assert _annotations_of(mcp, name).readOnlyHint is True
    # cancel mutates job state -> no read-only hint.
    assert _annotations_of(mcp, "job_cancel").readOnlyHint is False


def test_job_tools_schema_has_no_workspace_param(
    mcp: FastMCP, job_tool_names: list[str]
) -> None:
    """Jobs are session-scoped; no ``workspace`` selector leaks into the schema."""
    for name in ("job_status", "job_result", "job_cancel"):
        props = _params_of(mcp, name).get("properties", {})
        assert "workspace" not in props
        assert "job_id" in props


def test_slow_command_schema_exposes_background_boolean(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Every SLOW_COMMANDS tool gains an optional ``background`` boolean."""
    for name in sorted(SLOW_COMMANDS):
        params = _params_of(mcp, name)
        props = params["properties"]
        assert props["background"]["type"] == "boolean"
        assert props["background"].get("default") is False
        # Optional: must not be in the required list.
        assert "background" not in params.get("required", [])


def test_non_slow_command_schema_has_no_background(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Read-only / instant tools keep a clean schema (no ``background``)."""
    for name in ("whoami", "list_hosts", "add_host"):
        props = _params_of(mcp, name).get("properties", {})
        assert "background" not in props


def test_make_wrapper_adds_background_param_only_for_slow_commands() -> None:
    """The synthesised signature carries ``background`` iff the command is slow."""
    import inspect

    from mtui.mcp.tools import _make_wrapper

    session_stub = MagicMock()

    run_parser = Command.registry["run"].argparser(__import__("sys"))
    run_wrapper = _make_wrapper(Command.registry["run"], run_parser, session_stub)
    run_sig = inspect.signature(run_wrapper)
    assert "background" in run_sig.parameters
    bg = run_sig.parameters["background"]
    assert bg.kind is inspect.Parameter.KEYWORD_ONLY
    assert bg.default is False
    assert bg.annotation is bool

    who_parser = Command.registry["whoami"].argparser(__import__("sys"))
    who_wrapper = _make_wrapper(Command.registry["whoami"], who_parser, session_stub)
    assert "background" not in inspect.signature(who_wrapper).parameters


def test_slow_command_background_true_starts_job(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Calling a slow tool with ``background=True`` returns a job-id message.

    The backgrounded ``run`` starts an asyncio task; the wrapper returns
    immediately with a poll/fetch hint instead of the command's stdout.
    Driven with no hosts so the underlying ``run`` finishes fast.
    """
    tool = mcp._tool_manager.get_tool("run")  # noqa: SLF001
    assert tool is not None

    async def _call() -> str:
        return await tool.fn(command=["true"], hosts=[], background=True)

    out = asyncio.run(_call())
    assert "started job" in out
    assert "run-1" in out
    assert "job_status" in out
    assert "job_result" in out


# --------------------------------------------------------------------------- #
# Fan-out tools expose the template selection parameters (Phase 4)            #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("name", ["run", "update", "export"])
def test_fanout_tool_exposes_template_params(
    mcp: FastMCP, registered_names: list[str], name: str
) -> None:
    """Fan-out tools surface ``template`` / ``all_templates``, neither required.

    ``_add_template_arg`` adds these as ordinary argparse arguments, so the
    schema synthesiser exposes them automatically — a call without
    ``template`` fans out across the session's loaded templates, while
    ``template="<rrid>"`` scopes to one. This locks that surface in so a
    future refactor cannot silently drop it.
    """
    params = _params_of(mcp, name)
    props = params["properties"]
    assert "template" in props
    assert "all_templates" in props
    required = params.get("required", [])
    assert "template" not in required
    assert "all_templates" not in required
