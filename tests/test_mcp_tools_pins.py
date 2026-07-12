"""Mutation-killing pins for the runtime wiring in :mod:`mtui.mcp.tools`.

A full mutmut run showed that no test ever *invokes* a generated
wrapper with a request ``ctx`` present, calls the fanned-out
``config_show``/``config_set`` tools, or reads any tool's
``description`` — so mutants that drop the per-request Context, the
session provider, the subcommand argv prefix, or the tool description
all survived. The tests here drive the registered tools end to end
against a spy session/provider and pin:

* ``ctx`` is stripped from kwargs and forwarded (not discarded) into
  both session resolution and ``run_command``/``start_jobs``;
* the subparser fan-out passes the real provider and prepends the
  subcommand to the argv (``config_set`` -> ``['set', attr, value]``);
* tool descriptions come from the command docstring / subparser usage;
* slow commands run in the foreground unless ``background=True``, and
  the multi-job background message names every started job;
* the synthetic "exactly one required" guard reports the supplied
  names and count, and is NOT enforced for non-required groups.

Each test was verified to fail against a hand-applied representative
mutant before the pristine code was restored.
"""

from __future__ import annotations

import argparse
import asyncio
from types import SimpleNamespace
from typing import Any, cast

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

from mtui.commands import Command  # noqa: E402
from mtui.mcp.registry import DEFAULT_SESSION_KEY  # noqa: E402
from mtui.mcp.session import McpCommandError  # noqa: E402
from mtui.mcp.tools import _make_wrapper, build_tools  # noqa: E402

# --------------------------------------------------------------------------- #
# Spy plumbing                                                                #
# --------------------------------------------------------------------------- #


class SpySession:
    """Session + provider double recording every dispatch.

    Mirrors the dual shape of :class:`McpSession` (which is both the
    stdio session provider and the session itself): ``get_or_create``
    records the resolution key and returns ``self``; ``run_command`` /
    ``start_jobs`` record ``(cls, argv, ctx)`` without touching hosts.
    """

    def __init__(self) -> None:
        self.keys: list[str] = []
        self.run_calls: list[tuple[type, list[str], Any]] = []
        self.job_calls: list[tuple[type, list[str], Any]] = []
        self.result = "SPY-RESULT"
        self.job_ids = ["job-1"]

    async def get_or_create(self, key: str) -> Any:
        # ``Any`` return: the SessionProvider protocol promises an
        # ``McpSession``; this double records the key and stands in for it.
        self.keys.append(key)
        return self

    async def run_command(
        self, cmd_cls: type, argv: list[str], ctx: Any | None = None
    ) -> str:
        self.run_calls.append((cmd_cls, argv, ctx))
        return self.result

    async def start_jobs(
        self, cmd_cls: type, argv: list[str], *, ctx: Any | None = None
    ) -> list[str]:
        self.job_calls.append((cmd_cls, argv, ctx))
        return list(self.job_ids)


@pytest.fixture
def spy() -> SpySession:
    return SpySession()


@pytest.fixture
def mcp() -> FastMCP:
    return FastMCP(name="mtui-pins")


@pytest.fixture
def registered_names(mcp: FastMCP, spy: SpySession) -> list[str]:
    return build_tools(mcp, spy)


def _tool(mcp: FastMCP, name: str):
    tool = mcp._tool_manager.get_tool(name)  # noqa: SLF001
    assert tool is not None, f"tool {name!r} not registered"
    return tool


def _fake_ctx() -> SimpleNamespace:
    """Minimal stand-in for the FastMCP request Context.

    Only ``.session`` is read (by the registry's session-key
    derivation), so a bare namespace suffices.
    """
    return SimpleNamespace(session=object())


# --------------------------------------------------------------------------- #
# ctx forwarding                                                              #
# --------------------------------------------------------------------------- #


def test_wrapper_forwards_ctx_to_session_resolution_and_run(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """A supplied ``ctx`` reaches both ``get_or_create`` and ``run_command``.

    This is the progress-heartbeat contract from the module docstring:
    discarding ``ctx`` (or popping the wrong key) would resolve the
    default session and silence ``notifications/progress`` for every
    long-running command.
    """
    ctx = _fake_ctx()
    tool = _tool(mcp, "whoami")

    out = asyncio.run(tool.fn(ctx=ctx))

    assert out == "SPY-RESULT"
    assert spy.keys == [str(id(ctx.session))]
    assert len(spy.run_calls) == 1
    cls, argv, forwarded_ctx = spy.run_calls[0]
    assert cls is Command.registry["whoami"]
    assert argv == []
    assert forwarded_ctx is ctx


def test_wrapper_without_ctx_uses_default_session_key(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """Direct calls without a request Context resolve the default session."""
    asyncio.run(_tool(mcp, "whoami").fn())
    assert spy.keys == [DEFAULT_SESSION_KEY]
    assert spy.run_calls[0][2] is None


def test_wrapper_strips_ctx_from_encoded_argv(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """``ctx`` never leaks into the argv handed to the command parser."""
    ctx = _fake_ctx()
    asyncio.run(_tool(mcp, "add_host").fn(target=["h1"], ctx=ctx))

    _, argv, forwarded_ctx = spy.run_calls[0]
    assert argv == ["--target", "h1"]
    assert forwarded_ctx is ctx


# --------------------------------------------------------------------------- #
# Subparser fan-out runtime behaviour                                          #
# --------------------------------------------------------------------------- #


def test_config_set_invocation_prepends_subcommand(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """``config_set`` dispatches ``['set', attribute, value]`` via the provider.

    Registration alone cannot catch a dropped ``argv_prefix`` or a
    ``None`` provider — only invoking the wrapper does.
    """
    out = asyncio.run(_tool(mcp, "config_set").fn(attribute="session_user", value="x"))

    assert out == "SPY-RESULT"
    cls, argv, _ = spy.run_calls[0]
    assert cls is Command.registry["config"]
    assert argv == ["set", "session_user", "x"]


def test_config_show_invocation_prepends_subcommand(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """``config_show`` dispatches ``['show', *attributes]``."""
    asyncio.run(_tool(mcp, "config_show").fn(attributes=["session_user"]))

    cls, argv, _ = spy.run_calls[0]
    assert cls is Command.registry["config"]
    assert argv == ["show", "session_user"]


# --------------------------------------------------------------------------- #
# Tool descriptions                                                           #
# --------------------------------------------------------------------------- #


def test_tool_description_is_stripped_command_docstring(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """Ordinary tools advertise the command class docstring, stripped."""
    for name in ("run", "update", "whoami"):
        expected = (Command.registry[name].__doc__ or "").strip()
        assert expected, f"command {name!r} lost its docstring"
        assert _tool(mcp, name).description == expected


def test_fanned_out_tool_description_names_the_subcommand(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """``config_show``/``config_set`` descriptions come from the subparser.

    A dropped description would silently fall back to the parent
    ``config`` docstring, misleading clients about which tool does what.
    """
    assert _tool(mcp, "config_show").description.startswith("usage: config show")
    assert _tool(mcp, "config_set").description.startswith("usage: config set")


# --------------------------------------------------------------------------- #
# background flag semantics                                                    #
# --------------------------------------------------------------------------- #


def test_slow_tool_runs_foreground_by_default(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """Omitting ``background`` runs the command inline, not as a job."""
    out = asyncio.run(_tool(mcp, "run").fn(command=["ls"], hosts=[]))

    assert out == "SPY-RESULT"
    assert spy.job_calls == []
    assert len(spy.run_calls) == 1


def test_slow_tool_background_multi_job_message_lists_every_job(
    mcp: FastMCP, spy: SpySession, registered_names: list[str]
) -> None:
    """Fanning out to several templates reports every job id.

    ``start_jobs`` mints one job per loaded template; the multi-job
    branch of the wrapper (untested before) must name the command, the
    job count, and each id so the client can poll them all.
    """
    spy.job_ids = ["run-1", "run-2"]
    ctx = _fake_ctx()

    out = asyncio.run(
        _tool(mcp, "run").fn(command=["ls"], hosts=[], background=True, ctx=ctx)
    )

    assert spy.run_calls == []
    cls, argv, forwarded_ctx = spy.job_calls[0]
    assert cls is Command.registry["run"]
    assert argv == ["ls"]
    assert forwarded_ctx is ctx
    assert "started 2 jobs" in out
    assert "`run`" in out
    assert "'run-1'" in out
    assert "'run-2'" in out
    assert "job_status" in out


# --------------------------------------------------------------------------- #
# Synthetic "exactly one required" runtime guard                              #
# --------------------------------------------------------------------------- #


def test_synthetic_required_error_reports_supplied_names(
    mcp: FastMCP, registered_names: list[str]
) -> None:
    """The violation message carries the real count and kwarg names."""
    with pytest.raises(McpCommandError) as both:
        asyncio.run(
            _tool(mcp, "load_template").fn(
                auto_review_id="SUSE:Maintenance:1:1",
                kernel_review_id="SUSE:Maintenance:2:2",
            )
        )
    assert both.value.exit_code == 2
    assert "--auto-review-id, --kernel-review-id" in both.value.stderr
    assert "got 2 (auto_review_id, kernel_review_id)" in both.value.stderr

    with pytest.raises(McpCommandError) as neither:
        asyncio.run(
            _tool(mcp, "load_template").fn(auto_review_id=None, kernel_review_id=None)
        )
    assert "got 0 (none)" in neither.value.stderr


def test_non_required_synthetic_group_is_not_enforced(spy: SpySession) -> None:
    """Only a *required* mutex group triggers the exactly-one guard.

    A parser with a non-required shared-dest StoreAction group plus an
    unrelated required group must accept a call supplying neither
    synthetic kwarg: the guard has to key off the synthetic group's own
    ``required`` flag (intersection, not union, with the group members).
    """

    class ProbeCommand:
        """Probe command for wrapper synthesis."""

        command = "probe_pin"

    parser = argparse.ArgumentParser(prog="probe_pin")
    optional_group = parser.add_mutually_exclusive_group()
    optional_group.add_argument("--alpha-id", dest="ident")
    optional_group.add_argument("--beta-id", dest="ident")
    required_group = parser.add_mutually_exclusive_group(required=True)
    required_group.add_argument("--on", action="store_true")
    required_group.add_argument("--off", action="store_true")

    wrapper = _make_wrapper(cast("type[Command]", ProbeCommand), parser, spy)

    out = asyncio.run(wrapper(on=True))

    assert out == "SPY-RESULT"
    assert len(spy.run_calls) == 1
    _, argv, _ = spy.run_calls[0]
    assert argv == ["--on"]

    # The synthetic kwargs still route when supplied.
    asyncio.run(wrapper(alpha_id="X"))
    assert spy.run_calls[1][1] == ["--alpha-id", "X"]
