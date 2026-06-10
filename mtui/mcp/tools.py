"""Synthesise MCP tools from :class:`mtui.commands.Command` subclasses.

For every concrete command registered in :attr:`Command.registry` that
is not on the REPL-only deny-list, this module builds one MCP tool
whose:

* **name** matches the command's ``command`` attribute (e.g. ``run``).
* **description** comes from the command's docstring.
* **input schema** is inferred by :mod:`mcp.server.fastmcp` from a
  synthesised function signature built by
  :func:`mtui.mcp._schema.build_parameters`.
* **handler** re-serialises kwargs to argv via
  :func:`mtui.mcp._argv.kwargs_to_argv` and dispatches through
  :meth:`McpSession.run_command`, which serialises calls behind the
  session-wide lock and captures stdout/stderr.

Subparser commands (only ``config`` today) are fanned out: one tool per
subcommand (``config_show``, ``config_set``). The bare ``config`` tool
is not registered because the schema for "either show OR set" cannot
be expressed in a single object schema without lying about which
fields are required.

``readOnlyHint`` is set conservatively via a prefix allow-list — see
:data:`_READ_ONLY_PREFIXES` and :data:`_READ_ONLY_EXACT`. Destructive
commands (``approve``, ``update``, ``reject``, ...) get no hint, which
LLM clients treat as "may have side effects".
"""

from __future__ import annotations

import inspect
from logging import getLogger
from typing import TYPE_CHECKING, Any

from ..commands import Command
from ._argv import kwargs_to_argv
from ._schema import build_parameters
from .deny import REPL_ONLY
from .registry import resolve_session
from .session import McpCommandError

if TYPE_CHECKING:
    import argparse

    # ``Context`` is imported lazily inside ``_make_wrapper`` (it requires
    # the optional ``mcp`` extra); the TYPE_CHECKING re-export is kept for
    # editors/type-checkers, hence the ``noqa: F401``.
    from mcp.server.fastmcp import Context, FastMCP  # noqa: F401

    from .registry import SessionProvider

logger = getLogger("mtui.mcp.tools")

#: Command names whose ``_check_subparser`` we fan out into one tool
#: per subcommand. Pinned here (rather than auto-discovered) so the
#: list is stable and visible in code review.
SUBPARSER_COMMANDS: frozenset[str] = frozenset({"config"})

#: A command becomes ``readOnlyHint=True`` if its name starts with one
#: of these prefixes. Strict allow-list per Q2 in PLAN step 7.
_READ_ONLY_PREFIXES: tuple[str, ...] = ("list_", "show_")

#: Exact names that escape the prefix rule but are still side-effect-free.
_READ_ONLY_EXACT: frozenset[str] = frozenset({"whoami", "products", "openqa_overview"})


def _is_read_only(name: str) -> bool:
    """Return ``True`` iff a command is known to be side-effect-free."""
    if name in _READ_ONLY_EXACT:
        return True
    return any(name.startswith(p) for p in _READ_ONLY_PREFIXES)


def _make_wrapper(
    cls: type[Command],
    parser: argparse.ArgumentParser,
    provider: SessionProvider,
    argv_prefix: tuple[str, ...] = (),
):
    """Build the async tool handler for ``cls``.

    The returned coroutine carries the synthesised signature so the
    MCP server can infer the input schema. ``argv_prefix`` is
    prepended to the argv produced from kwargs; used by the subparser
    fan-out to inject the subcommand name (``["show"]`` for
    ``config_show``).

    The synthesised signature also carries a trailing keyword-only
    ``ctx`` parameter annotated as :class:`mcp.server.fastmcp.Context`.
    FastMCP detects the ``Context`` annotation via
    :func:`mcp.server.fastmcp.utilities.context_injection.find_context_parameter`
    (which reads :func:`typing.get_type_hints`), strips the parameter
    from the JSON schema via ``skip_names``, and injects the live
    per-request :class:`Context` at call time. We use it twice: to
    resolve the caller's isolated :class:`McpSession` from ``provider``
    (keyed on the request session — see
    :func:`mtui.mcp.registry.resolve_session`) and to let
    :meth:`McpSession.run_command` heartbeat ``notifications/progress``
    while the worker thread runs, which keeps MCP clients from timing
    out on long-running commands.
    """
    # Import lazily and only when actually building wrappers: callers
    # outside the ``mtui[mcp]`` extra never reach this function (the
    # MCP entrypoint already errored out with a friendly hint), but
    # the lazy import keeps ``mtui.mcp.tools`` importable in
    # documentation builds and unit tests that monkey-patch around
    # the SDK.
    from mcp.server.fastmcp import Context

    params = build_parameters(parser)
    # Append the Context-typed parameter LAST so the existing required-
    # before-optional ordering is preserved (``ctx`` has a default of
    # ``None`` and is keyword-only).
    ctx_param = inspect.Parameter(
        "ctx",
        inspect.Parameter.KEYWORD_ONLY,
        default=None,
        annotation=Context,
    )
    all_params = [*params, ctx_param]
    signature = inspect.Signature(
        parameters=all_params,
        return_annotation=str,
    )
    annotations = {p.name: p.annotation for p in all_params}
    annotations["return"] = str

    # Synthetic-name mutex groups (load_template -a/-k) need a runtime
    # "exactly one required" check: each synthetic kwarg is optional in
    # the JSON schema so the LLM can call either one, but the
    # underlying argparse mutex group is ``required=True``. Surfacing
    # the violation here gives a clean one-line stderr instead of an
    # argparse usage dump.
    synthetic_dests: dict[str, Any] = getattr(parser, "_mtui_synthetic_dests", {})
    synthetic_required = synthetic_dests and any(
        g.required
        for g in parser._mutually_exclusive_groups  # noqa: SLF001
        if {id(a) for a in g._group_actions}  # noqa: SLF001
        & {id(a) for a in synthetic_dests.values()}
    )
    synthetic_names = tuple(synthetic_dests.keys())
    synthetic_flags = ", ".join(
        a.option_strings[-1] for a in synthetic_dests.values() if a.option_strings
    )

    async def wrapper(**kwargs: Any) -> str:
        # FastMCP injects ``ctx`` only when the client request carries
        # a progress token (and even then, only because we annotated
        # the parameter as Context); strip it before kwargs_to_argv so
        # the encoder does not try to render it as a CLI flag.
        ctx = kwargs.pop("ctx", None)
        if synthetic_required:
            supplied = [n for n in synthetic_names if kwargs.get(n) is not None]
            if len(supplied) != 1:
                msg = (
                    f"exactly one of {synthetic_flags} is required; "
                    f"got {len(supplied)} ({', '.join(supplied) or 'none'})"
                )
                raise McpCommandError("", msg, 2)
        argv = list(argv_prefix) + kwargs_to_argv(parser, kwargs)
        session = await resolve_session(provider, ctx)
        return await session.run_command(cls, argv, ctx=ctx)

    # ``inspect.Signature`` lives on a dunder slot; ty does not model it
    # for nested ``async def``, hence the suppression.
    wrapper.__signature__ = signature  # ty: ignore[unresolved-attribute]
    wrapper.__annotations__ = annotations
    wrapper.__name__ = f"tool_{cls.command}"
    wrapper.__doc__ = cls.__doc__
    return wrapper


def _register_tool(
    mcp: FastMCP,
    *,
    name: str,
    cls: type[Command],
    parser: argparse.ArgumentParser,
    provider: SessionProvider,
    argv_prefix: tuple[str, ...] = (),
    description: str | None = None,
) -> None:
    """Build a wrapper and register it on ``mcp`` as a tool.

    The SDK's :meth:`FastMCP.add_tool` accepts the callable directly
    and synthesises the JSON Schema from its signature; there is no
    separate ``FunctionTool`` wrapper to construct.
    """
    from mcp.types import ToolAnnotations

    wrapper = _make_wrapper(cls, parser, provider, argv_prefix=argv_prefix)
    desc = (description or cls.__doc__ or name).strip()
    mcp.add_tool(
        wrapper,
        name=name,
        description=desc,
        annotations=ToolAnnotations(readOnlyHint=_is_read_only(name)),
    )


def _fan_out_subparser(
    mcp: FastMCP,
    cls: type[Command],
    provider: SessionProvider,
) -> list[str]:
    """Register one tool per subcommand of a parser using ``add_subparsers``.

    Returns the list of registered tool names. The bare parent name is
    NOT registered: a "show or set" union schema would mislead the LLM
    about which fields are required.
    """
    import argparse

    parser = cls.argparser(__import__("sys"))
    sub_action = next(
        (a for a in parser._actions if isinstance(a, argparse._SubParsersAction)),
        None,
    )
    if sub_action is None:  # pragma: no cover - defensive: SUBPARSER_COMMANDS lies
        logger.warning(
            "command %r is in SUBPARSER_COMMANDS but has no subparsers; skipping",
            cls.command,
        )
        return []

    names: list[str] = []
    for subname, sub_parser in sub_action.choices.items():
        tool_name = f"{cls.command}_{subname}"
        desc = (
            sub_parser.description or sub_parser.format_help() or cls.__doc__ or ""
        ).strip()
        _register_tool(
            mcp,
            name=tool_name,
            cls=cls,
            parser=sub_parser,
            provider=provider,
            argv_prefix=(subname,),
            description=desc.splitlines()[0] if desc else tool_name,
        )
        names.append(tool_name)
    return names


def build_tools(mcp: FastMCP, provider: SessionProvider) -> list[str]:
    """Register one MCP tool per non-denied :class:`Command` subclass.

    Subparser commands (``config`` today) are fanned out into one tool
    per subcommand. The deny-list intersection is asserted on entry so
    a renamed deny-listed command fails loudly at boot rather than
    silently exposing a REPL-only command as a tool.

    Args:
        mcp: The :class:`mcp.server.fastmcp.FastMCP` server instance
            to register tools on.
        provider: The :class:`mtui.mcp.registry.SessionProvider` every
            tool resolves its per-call :class:`McpSession` through —
            a :class:`mtui.mcp.registry.SessionRegistry` under http
            (one isolated session per client) or a single
            :class:`McpSession` under stdio.

    Returns:
        Sorted list of registered tool names (used by the boot log and
        in tests).

    """
    # Defensive: if REPL_ONLY drifts away from the live registry the
    # operator should see it at boot, not weeks later in production.
    deny_present = REPL_ONLY & set(Command.registry)
    if deny_present != REPL_ONLY:
        missing = REPL_ONLY - deny_present
        logger.warning(
            "deny-list entries missing from Command.registry: %s; "
            "rename or remove the stale entries in mtui.mcp.deny",
            sorted(missing),
        )

    registered: list[str] = []
    for name, cls in sorted(Command.registry.items()):
        if name in REPL_ONLY:
            continue
        if name in SUBPARSER_COMMANDS:
            registered.extend(_fan_out_subparser(mcp, cls, provider))
            continue
        parser = cls.argparser(__import__("sys"))
        _register_tool(
            mcp,
            name=name,
            cls=cls,
            parser=parser,
            provider=provider,
        )
        registered.append(name)

    registered.sort()
    logger.info("registered %d MCP tools: %s", len(registered), ", ".join(registered))
    return registered
