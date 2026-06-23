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

#: Commands that touch the reference hosts and can run for minutes. These
#: gain a ``background`` parameter: when true the call returns a job id at
#: once (see :meth:`McpSession.start_job`) instead of holding the request
#: open, so a slow host op does not stop the client from issuing other
#: calls. Polled via the ``job_status`` / ``job_result`` tools.
SLOW_COMMANDS: frozenset[str] = frozenset(
    {
        "run",
        "update",
        "downgrade",
        "prepare",
        "install",
        "uninstall",
        "set_repo",
        "reboot",
    }
)

#: A command becomes ``readOnlyHint=True`` if its name starts with one
#: of these prefixes. Strict allow-list per Q2 in PLAN step 7.
_READ_ONLY_PREFIXES: tuple[str, ...] = ("list_", "show_")

#: Exact names that escape the prefix rule but are still side-effect-free.
#: (``openqa_overview`` and ``openqa_jobs`` only query openQA; ``reload_products``
#: is intentionally absent — it re-reads products from the hosts, a side effect.)
_READ_ONLY_EXACT: frozenset[str] = frozenset(
    {"whoami", "openqa_overview", "openqa_jobs"}
)


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
    # Slow host commands gain a ``background`` flag: when true the call
    # returns a job id immediately instead of blocking (see
    # ``McpSession.start_job``). Only added for SLOW_COMMANDS so read-only
    # / instant tools keep a clean schema.
    is_slow = cls.command in SLOW_COMMANDS
    extra_params: list[inspect.Parameter] = []
    if is_slow:
        extra_params.append(
            inspect.Parameter(
                "background",
                inspect.Parameter.KEYWORD_ONLY,
                default=False,
                annotation=bool,
            )
        )
    all_params = [*params, *extra_params, ctx_param]
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
        background = bool(kwargs.pop("background", False)) if is_slow else False
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
        if background:
            job_id = await session.start_job(cls, argv, ctx=ctx)
            return (
                f"started background job {job_id!r} for `{cls.command}`; it "
                f"runs on the hosts while you work elsewhere. Poll "
                f"job_status(job_id={job_id!r}) and fetch output with "
                f"job_result(job_id={job_id!r})."
            )
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


def register_job_tools(mcp: FastMCP, provider: SessionProvider) -> list[str]:
    """Register the background-job control tools (the async slow-op path).

    Slow host commands (``run``/``update``/``downgrade``/...) accept a
    ``background=true`` flag that returns a job id immediately instead of
    blocking; these tools then drive that job:

    * ``job_list`` — every job in the session and its state;
    * ``job_status`` — one job's state + elapsed time;
    * ``job_result`` — a finished job's output (errors if still running, or
      surfaces the command's failure envelope if it failed);
    * ``job_cancel`` — cancel a running job.

    They address the per-call session's job table, resolved through
    ``provider`` exactly as every other tool resolves its session, so they
    stay transport-agnostic (one session under stdio; the caller's isolated
    session under http).

    Args:
        mcp: The :class:`mcp.server.fastmcp.FastMCP` server.
        provider: The session provider each call resolves through.

    Returns:
        The registered tool names.

    """
    # The trailing ``ctx: Context | None = None`` parameter on each tool
    # is what FastMCP's ``find_context_parameter`` picks up via
    # :func:`typing.get_type_hints` to strip ``ctx`` from the JSON schema
    # and inject the live per-request Context. ``get_type_hints`` (and
    # FastMCP's ``inspect.signature(fn, eval_str=True)``) resolve the
    # string annotation ``"Context | None"`` against this *module's*
    # globals, not the enclosing function's locals, so bind ``Context``
    # here rather than importing it at module top — keeping
    # ``mtui.mcp.tools`` importable without the ``[mcp]`` extra. Bind
    # unconditionally (not ``setdefault``) so a stale stand-in left in
    # globals by an earlier caller never shadows the real SDK type.
    from mcp.server.fastmcp import Context as _Context
    from mcp.types import ToolAnnotations

    globals()["Context"] = _Context

    async def job_list(ctx: Context | None = None) -> str:
        """List background jobs in this session and their state."""
        session = await resolve_session(provider, ctx)
        jobs = session.job_list()
        if not jobs:
            return "no background jobs"
        return "\n".join(
            f"- {j['id']}: {j['state']} ({j['elapsed_s']}s) [{j['command']}]"
            for j in jobs
        )

    async def job_status(job_id: str, ctx: Context | None = None) -> str:
        """Report one background job's state and elapsed time."""
        session = await resolve_session(provider, ctx)
        j = session.job_status(job_id)
        return f"{j['id']}: {j['state']} ({j['elapsed_s']}s) [{j['command']}]"

    async def job_result(job_id: str, ctx: Context | None = None) -> str:
        """Return a finished job's output (error if not yet done)."""
        session = await resolve_session(provider, ctx)
        return session.job_result(job_id)

    async def job_cancel(job_id: str, ctx: Context | None = None) -> str:
        """Cancel a running background job."""
        session = await resolve_session(provider, ctx)
        return await session.job_cancel(job_id)

    list_desc = (
        "List background jobs in this session (started by calling a slow host "
        "command — run/update/downgrade/prepare/install/uninstall/set_repo/"
        "reboot — with background=true) and their state."
    )
    status_desc = (
        "Report a background job's state (running/done/failed/cancelled) and "
        "elapsed time. Poll this after starting a slow command with "
        "background=true."
    )
    result_desc = (
        "Return a finished background job's output. Errors if the job is still "
        "running (poll job_status first) or surfaces the command's failure if "
        "it failed."
    )
    cancel_desc = (
        "Cancel a running background job. Note: a job already executing on a "
        "host (SSH/subprocess) may keep running on the host even after cancel."
    )

    mcp.add_tool(
        job_list,
        name="job_list",
        description=list_desc,
        annotations=ToolAnnotations(readOnlyHint=True),
    )
    mcp.add_tool(
        job_status,
        name="job_status",
        description=status_desc,
        annotations=ToolAnnotations(readOnlyHint=True),
    )
    mcp.add_tool(
        job_result,
        name="job_result",
        description=result_desc,
        annotations=ToolAnnotations(readOnlyHint=True),
    )
    mcp.add_tool(
        job_cancel,
        name="job_cancel",
        description=cancel_desc,
        annotations=ToolAnnotations(readOnlyHint=False),
    )
    logger.info("registered 4 job tools: job_cancel, job_list, job_result, job_status")
    return ["job_cancel", "job_list", "job_result", "job_status"]
