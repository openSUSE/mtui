"""Hand-written MCP tools for editing the loaded testreport file.

The auto-generated tools synthesised by :mod:`mtui.mcp.tools` cover
every :class:`mtui.commands.Command` subclass, but the REPL ``edit``
command spawns ``$EDITOR`` on ``metadata.path`` — meaningless under
MCP. This module replaces it with three explicit tools that operate
directly on the file path tracked by ``session.metadata.path``:

* :func:`testreport_read` — return the file's bytes plus a line count
  computed with the same convention :func:`testreport_patch` consumes.
* :func:`testreport_patch` — splice an inclusive 1-indexed line range,
  written atomically via ``NamedTemporaryFile`` + :func:`os.replace`.
* :func:`testreport_write` — full-file overwrite, also atomic.

All three are ``async def``. The registered tool closures resolve the
caller's own :class:`McpSession` from the
:class:`mtui.mcp.registry.SessionProvider` (keyed on the request
session under http, a single session under stdio) and then acquire
*that session's* ``_lock``, so each client serialises only against its
own concurrent :meth:`McpSession.run_command` calls and patches **only
its own** loaded testreport file — no cross-client interference.

Refusal is uniform: when ``session.metadata`` is a
:class:`NullTestReport` or ``metadata.path`` is ``None``, every tool
raises :class:`mtui.mcp.session.McpCommandError` carrying an empty
stdout, a one-sentence stderr, and ``exit_code=1`` — the same envelope
the auto-generated tools use, so the MCP server layer surfaces the
same error shape across the whole MCP surface.
"""

from __future__ import annotations

import contextlib
import os
import tempfile
from logging import getLogger
from pathlib import Path
from typing import TYPE_CHECKING, Any

from ..test_reports.null_report import NullTestReport
from .registry import resolve_session
from .session import McpCommandError

if TYPE_CHECKING:
    from mcp.server.fastmcp import Context, FastMCP

    from .registry import SessionProvider
    from .session import McpSession

logger = getLogger("mtui.mcp.testreport_tools")

#: The warning glued onto every tool description so the LLM sees it.
_READ_FIRST_WARNING = (
    "Always call `testreport_read` immediately before `testreport_patch` "
    "to get current line numbers; line numbers shift after every patch."
)


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


def _resolve_testreport_path(session: McpSession) -> Path:
    """Return the loaded testreport's path or raise :class:`McpCommandError`.

    Two refusal conditions, both producing the same single-sentence
    error message so the LLM does not have to disambiguate:

    * ``session.metadata`` is a :class:`NullTestReport` (no template
      ever loaded in this session).
    * ``session.metadata.path`` is ``None`` (loaded but no on-disk
      representation, defensive — should not happen in practice).

    Args:
        session: The active :class:`McpSession`.

    Returns:
        The :class:`pathlib.Path` pointing at the loaded testreport.

    Raises:
        McpCommandError: When no testreport is loaded.

    """
    metadata = session.metadata
    if isinstance(metadata, NullTestReport) or metadata.path is None:
        raise McpCommandError(
            "",
            "no testreport loaded; run `load_template` first",
            1,
        )
    return Path(metadata.path)


def _atomic_write_text(path: Path, text: str) -> int:
    """Atomically replace ``path`` with ``text`` (utf-8 encoded).

    The new contents land in a sibling :class:`NamedTemporaryFile`,
    are flushed and :func:`os.fsync`-ed, then swapped into place with
    :func:`os.replace`. On any failure the temporary file is unlinked
    (best-effort) and the original exception re-raised, so the
    on-disk file is either fully old or fully new — never torn — and
    ``path.parent`` is free of ``tmp*`` residue.

    Args:
        path: Destination path. The parent directory must already
            exist and be writable.
        text: New file contents.

    Returns:
        The number of bytes written (utf-8 encoded length).

    """
    encoded = text.encode("utf-8")
    tmp_path: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
            mode="wb",
        ) as tmp:
            tmp_path = tmp.name
            tmp.write(encoded)
            tmp.flush()
            os.fsync(tmp.fileno())
        os.replace(tmp_path, path)
    except BaseException:
        if tmp_path is not None:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(tmp_path)
        raise
    return len(encoded)


def _count_lines(text: str) -> int:
    """Count lines using the same convention :func:`splitlines` uses.

    ``"foo\\nbar\\n"`` → 2; ``"foo\\nbar"`` → 2; ``""`` → 0. Shared by
    :func:`testreport_read` and :func:`testreport_patch`/:func:`testreport_write`
    so callers cannot observe a count drift between the two surfaces.
    """
    if not text:
        return 0
    return len(text.splitlines())


async def _heartbeat(ctx: Context | None, message: str) -> None:
    """Best-effort single progress frame for the testreport tools.

    Disk I/O on these tools is typically sub-second so a heartbeat
    loop would be overkill; we emit one frame *before* the I/O so
    queued clients can see the call has been picked up. ``ctx`` is
    optional (tests and direct coroutine callers pass ``None``); a
    notification-send failure is swallowed so a flaky transport never
    masks the actual tool outcome.
    """
    if ctx is None:
        return
    try:
        await ctx.report_progress(progress=0.0, total=None, message=message)
    except Exception as exc:  # noqa: BLE001 - never mask the tool result
        logger.debug("progress notification failed: %s", exc)


# --------------------------------------------------------------------------- #
# Tools                                                                       #
# --------------------------------------------------------------------------- #


async def testreport_read(
    session: McpSession,
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Return the loaded testreport file's content and line count.

    Acquires ``session._lock`` so the snapshot is coherent with any
    in-flight command dispatch (e.g. a concurrent ``commit``).
    """
    await _heartbeat(ctx, "testreport_read: waiting for session lock")
    async with session._lock:
        path = _resolve_testreport_path(session)
        content = path.read_text(encoding="utf-8", errors="replace")
    return {
        "path": str(path),
        "line_count": _count_lines(content),
        "content": content,
    }


async def testreport_patch(
    session: McpSession,
    start_line: int,
    end_line: int,
    replacement: str,
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Replace an inclusive 1-indexed line range with ``replacement``.

    ``end_line == start_line - 1`` is a pure insertion before
    ``start_line`` (e.g. ``start_line=1, end_line=0`` prepends).
    ``replacement`` is forced to end with a single newline iff
    non-empty so the splice preserves the trailing-newline invariant
    of the surrounding lines (which come from
    :meth:`str.splitlines` with ``keepends=True``).
    """
    await _heartbeat(ctx, "testreport_patch: waiting for session lock")
    async with session._lock:
        path = _resolve_testreport_path(session)
        content = path.read_text(encoding="utf-8", errors="replace")
        lines = content.splitlines(keepends=True)
        n = len(lines)
        if start_line < 1 or end_line < start_line - 1 or end_line > n:
            raise McpCommandError(
                "",
                (
                    f"line range out of bounds: start_line={start_line}, "
                    f"end_line={end_line}, file has {n} line(s)"
                ),
                1,
            )

        if replacement:
            normalized = (
                replacement if replacement.endswith("\n") else replacement + "\n"
            )
            new_segment = [normalized]
        else:
            new_segment = []

        new_lines = lines[: start_line - 1] + new_segment + lines[end_line:]
        new_text = "".join(new_lines)
        bytes_written = _atomic_write_text(path, new_text)

    replaced_lines = max(0, end_line - start_line + 1)
    inserted_lines = _count_lines(replacement) if replacement else 0
    return {
        "path": str(path),
        "new_line_count": _count_lines(new_text),
        "replaced_lines": replaced_lines,
        "inserted_lines": inserted_lines,
        "bytes_written": bytes_written,
    }


async def testreport_write(
    session: McpSession,
    content: str,
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Overwrite the loaded testreport file with ``content`` atomically.

    Fallback for the case where line drift across patches makes
    :func:`testreport_patch` unreliable; the LLM resends the whole
    file in one shot.
    """
    await _heartbeat(ctx, "testreport_write: waiting for session lock")
    async with session._lock:
        path = _resolve_testreport_path(session)
        bytes_written = _atomic_write_text(path, content)
    return {
        "path": str(path),
        "bytes_written": bytes_written,
        "line_count": _count_lines(content),
    }


# --------------------------------------------------------------------------- #
# Registration                                                                #
# --------------------------------------------------------------------------- #


def register_testreport_tools(mcp: FastMCP, provider: SessionProvider) -> list[str]:
    """Register the three testreport tools on ``mcp``.

    Mirrors the registration shape used by :func:`mtui.mcp.tools.build_tools`
    so the boot log entries look uniform.

    Args:
        mcp: The :class:`mcp.server.fastmcp.FastMCP` server.
        provider: The :class:`mtui.mcp.registry.SessionProvider` each
            tool resolves its per-call :class:`McpSession` through, so
            under http every client patches **only its own** loaded
            testreport file. Under stdio this is a single
            :class:`McpSession`.

    Returns:
        Sorted list of registered tool names — used by the boot log
        and asserted in tests.

    """
    # The trailing ``ctx: Context | None = None`` parameter is what
    # FastMCP's ``find_context_parameter`` picks up via
    # :func:`typing.get_type_hints` to (a) strip ``ctx`` from the
    # tool's JSON schema and (b) inject the live per-request Context
    # at call time. ``get_type_hints`` resolves the string annotation
    # ``"Context | None"`` against the *module* globals (not the
    # enclosing function's locals), so we inject ``Context`` into
    # this module's globals lazily here rather than importing it at
    # module top — keeping ``mtui.mcp.testreport_tools`` importable
    # without the ``[mcp]`` extra.
    from mcp.server.fastmcp import Context as _Context
    from mcp.types import ToolAnnotations

    globals().setdefault("Context", _Context)

    async def _read(ctx: Context | None = None) -> dict[str, Any]:
        session = await resolve_session(provider, ctx)
        return await testreport_read(session, ctx=ctx)

    async def _patch(
        start_line: int,
        end_line: int,
        replacement: str,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx)
        return await testreport_patch(
            session, start_line, end_line, replacement, ctx=ctx
        )

    async def _write(content: str, ctx: Context | None = None) -> dict[str, Any]:
        session = await resolve_session(provider, ctx)
        return await testreport_write(session, content, ctx=ctx)

    read_desc = (
        "Read the currently loaded testreport file. Returns the path, "
        "line count, and full content (utf-8, errors replaced). "
        f"{_READ_FIRST_WARNING}"
    )
    patch_desc = (
        "Splice an inclusive 1-indexed line range in the currently loaded "
        "testreport file. `end_line == start_line - 1` inserts before "
        "`start_line` without replacing anything. The write is atomic. "
        f"{_READ_FIRST_WARNING}"
    )
    write_desc = (
        "Overwrite the currently loaded testreport file with the given "
        "content. Atomic. Use this as the fallback when patching would "
        f"require tracking line-number drift across many edits. {_READ_FIRST_WARNING}"
    )

    specs = (
        (
            "testreport_read",
            _read,
            read_desc,
            ToolAnnotations(readOnlyHint=True, idempotentHint=True),
        ),
        (
            "testreport_patch",
            _patch,
            patch_desc,
            ToolAnnotations(),
        ),
        (
            "testreport_write",
            _write,
            write_desc,
            ToolAnnotations(),
        ),
    )

    names: list[str] = []
    for name, fn, desc, hints in specs:
        # ``structured_output=False`` keeps the wire shape quirks-
        # compatible with the standalone fastmcp era: clients receive
        # a single text content block (the dict's str repr) rather
        # than the SDK's auto-derived structured payload + output
        # schema. The dict return is still useful for tests that
        # invoke the coroutines directly.
        mcp.add_tool(
            fn,
            name=name,
            description=desc,
            annotations=hints,
            structured_output=False,
        )
        names.append(name)

    names.sort()
    logger.info("registered %d testreport tools: %s", len(names), ", ".join(names))
    return names
