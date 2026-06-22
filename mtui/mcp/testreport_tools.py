"""Hand-written MCP tools for editing the loaded testreport file.

The auto-generated tools synthesised by :mod:`mtui.mcp.tools` cover
every :class:`mtui.commands.Command` subclass, but the REPL ``edit``
command spawns ``$EDITOR`` on ``metadata.path`` — meaningless under
MCP. This module replaces it with three explicit tools that operate
directly on the file path tracked by ``session.metadata.path``:

* :func:`testreport_read` — return the file's bytes plus a line count
  computed with the same convention :func:`testreport_patch` consumes.
  Optional ``offset``/``limit`` read a 1-indexed line window so a large
  report (a Product Increment log) can be paged instead of overflowing.
* :func:`testreport_patch` — splice an inclusive 1-indexed line range,
  written atomically via ``NamedTemporaryFile`` + :func:`os.replace`.
* :func:`testreport_write` — full-file overwrite, also atomic.

Two further read-only tools expose the rest of the checkout that the
``log`` file does not cover:

* :func:`testreport_logs` — list the ``build_checks/`` and
  ``install_logs/`` files (name + size).
* :func:`testreport_read_file` — read any file under the checkout
  directory by relative path (traversal-guarded).

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
import re
import tempfile
from logging import getLogger
from pathlib import Path
from typing import TYPE_CHECKING, Any

from ..test_reports.null_report import NullTestReport
from .registry import WORKSPACE_DEFAULT, resolve_session
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


def _resolve_template_dir(session: McpSession) -> Path:
    """Return the loaded testreport's checkout directory.

    This is the parent of the ``log`` file (``metadata.path``) — the directory
    holding ``metadata.json``, ``source.diff``, ``patchinfo.xml`` and the
    ``build_checks/`` and ``install_logs/`` subdirectories. Raises the same
    refusal as :func:`_resolve_testreport_path` when nothing is loaded.
    """
    return _resolve_testreport_path(session).parent


def _safe_template_file(base: Path, relpath: str) -> Path:
    """Resolve ``relpath`` under ``base``, refusing anything that escapes it.

    Guards against ``..`` traversal and absolute paths so a tool call can only
    read files inside the loaded checkout directory.
    """
    base_resolved = base.resolve()
    target = (base_resolved / relpath).resolve()
    if target != base_resolved and base_resolved not in target.parents:
        raise McpCommandError(
            "",
            f"path {relpath!r} escapes the testreport directory",
            1,
        )
    return target


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
    offset: int = 1,  # noqa: PT028 - tool entrypoint, not a pytest test
    limit: int | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Return the loaded testreport file's content and line count.

    By default the whole file is returned. ``offset``/``limit`` request a
    1-indexed inclusive line window — the same numbering ``testreport_patch``
    consumes — so a large report (a Product Increment ``log`` runs to thousands
    of lines after ``export``) can be paged instead of overflowing the caller.
    ``offset`` is the first line to return (1-based, default 1); ``limit`` caps
    how many lines to return (default: to end of file). ``line_count`` is always
    the file's *total* line count so the caller can page; when a window was
    requested the reply also carries ``offset`` and ``returned_lines``.

    Acquires ``session._lock`` so the snapshot is coherent with any
    in-flight command dispatch (e.g. a concurrent ``commit``).
    """
    if offset < 1:
        raise McpCommandError("", f"offset must be >= 1 (got {offset})", 1)
    if limit is not None and limit < 0:
        raise McpCommandError("", f"limit must be >= 0 (got {limit})", 1)

    await _heartbeat(ctx, "testreport_read: waiting for session lock")
    async with session._lock:
        path = _resolve_testreport_path(session)
        content = path.read_text(encoding="utf-8", errors="replace")

    windowed = offset != 1 or limit is not None
    if not windowed:
        return {
            "path": str(path),
            "line_count": _count_lines(content),
            "content": content,
        }

    # 1-indexed line slicing matching testreport_patch's numbering.
    lines = content.splitlines(keepends=True)
    start = offset - 1
    sliced = lines[start:] if limit is None else lines[start : start + limit]
    return {
        "path": str(path),
        "line_count": len(lines),
        "offset": offset,
        "returned_lines": len(sliced),
        "content": "".join(sliced),
    }


async def testreport_logs(
    session: McpSession,
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """List the auxiliary log files in the loaded testreport's checkout.

    The build-check logs (per source package and arch) and the per-refhost
    install logs live in ``build_checks/`` and ``install_logs/`` next to the
    testreport, but are not part of the ``log`` file. This returns their
    names (and byte sizes) so a caller can then fetch one with
    :func:`testreport_read_file`.
    """
    await _heartbeat(ctx, "testreport_logs: waiting for session lock")
    async with session._lock:
        base = _resolve_template_dir(session)

        def listing(sub: str) -> list[dict[str, Any]]:
            d = base / sub
            if not d.is_dir():
                return []
            return [
                {"name": p.name, "size": p.stat().st_size}
                for p in sorted(d.iterdir())
                if p.is_file()
            ]

        return {
            "path": str(base),
            "build_checks": listing("build_checks"),
            "install_logs": listing("install_logs"),
        }


async def testreport_read_file(
    session: McpSession,
    relpath: str,
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Read a file from the loaded testreport's checkout by relative path.

    Complements :func:`testreport_read` (which only returns the ``log``):
    use this for ``build_checks/<pkg>.<arch>.log``, ``install_logs/<host>.log``,
    ``source.diff``, ``patchinfo.xml`` and the like. ``relpath`` is resolved
    under the checkout directory and may not escape it.
    """
    await _heartbeat(ctx, "testreport_read_file: waiting for session lock")
    async with session._lock:
        base = _resolve_template_dir(session)
        target = _safe_template_file(base, relpath)
        if not target.is_file():
            raise McpCommandError(
                "",
                f"no such file in testreport checkout: {relpath}",
                1,
            )
        content = target.read_text(encoding="utf-8", errors="replace")
    return {
        "path": str(target),
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
# Bulk placeholder fill                                                        #
# --------------------------------------------------------------------------- #

#: The exact placeholder tokens the QAM testreport template ships with, one per
#: line, that a tester otherwise has to flip one `testreport_patch` at a time.
#: Each regex captures the label+padding as ``pre`` so the replacement keeps the
#: template's column alignment; the value part is matched verbatim so an
#: already-filled line (e.g. ``STATUS:             SKIPPED``) is never touched —
#: the fill is idempotent and safe to re-run.
_SUMMARY_PLACEHOLDER = re.compile(r"^(?P<pre>\s*SUMMARY:\s*)PASSED/FAILED\s*$")
_REPRODUCER_PLACEHOLDER = re.compile(r"^(?P<pre>\s*REPRODUCER_PRESENT:\s*)YES/NO\s*$")
_STATUS_PLACEHOLDER = re.compile(
    r"^(?P<pre>\s*STATUS:\s*)"
    r"FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/"
    r"NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER\s*$"
)

#: Valid single-value codes for the per-bug ``STATUS:`` field.
_STATUS_CODES = frozenset(
    {
        "FIXED",
        "NOT_FIXED",
        "HYPOTHETICAL",
        "NOT_REPRODUCIBLE",
        "NO_ENVIRONMENT",
        "TOO_COMPLEX",
        "SKIPPED",
        "OTHER",
    }
)


async def testreport_fill(
    session: McpSession,
    reproducer: str | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
    status: str | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
    summary: str | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
    ctx: Context | None = None,  # noqa: PT028 - tool entrypoint, not a pytest test
) -> dict[str, Any]:
    """Bulk-fill the repetitive placeholder tokens left by ``export``.

    A freshly exported QAM testreport carries one ``REPRODUCER_PRESENT: YES/NO``
    + ``STATUS: FIXED/.../OTHER`` placeholder per referenced bug (a CVE-heavy
    update can have 15+), plus the top ``SUMMARY: PASSED/FAILED``. Flipping each
    by hand with :func:`testreport_patch` is slow and error-prone on a live
    report. This sets them all in one atomic write.

    Only the **exact** template placeholder strings are replaced, so the call is
    idempotent and never clobbers a value you already filled in (e.g. a bug you
    set to ``REPRODUCER_PRESENT: YES`` / ``STATUS: FIXED`` by hand stays put).
    Typical use on a security update: ``reproducer="NO", status="SKIPPED",
    summary="PASSED"`` to clear every CVE placeholder at once, then override the
    handful of non-security bugs individually with :func:`testreport_patch`. The
    regression / build-log / source sections are still filled separately.

    Args:
        reproducer: ``YES`` or ``NO`` to set every unfilled
            ``REPRODUCER_PRESENT:`` line; ``None`` leaves them.
        status: a single ``STATUS:`` code (see :data:`_STATUS_CODES`) to set
            every unfilled templated ``STATUS:`` line; ``None`` leaves them.
        summary: ``PASSED`` or ``FAILED`` for the top ``SUMMARY:`` line;
            ``None`` leaves it.

    Returns:
        ``path``, a ``filled`` breakdown (how many of each token were set),
        ``bytes_written`` and ``line_count``.

    """
    if reproducer is not None and reproducer not in ("YES", "NO"):
        raise McpCommandError(
            "", f"reproducer must be YES or NO, got {reproducer!r}", 1
        )
    if status is not None and status not in _STATUS_CODES:
        raise McpCommandError(
            "", f"status must be one of {sorted(_STATUS_CODES)}, got {status!r}", 1
        )
    if summary is not None and summary not in ("PASSED", "FAILED"):
        raise McpCommandError(
            "", f"summary must be PASSED or FAILED, got {summary!r}", 1
        )
    if reproducer is None and status is None and summary is None:
        raise McpCommandError(
            "", "nothing to fill: pass at least one of reproducer/status/summary", 1
        )

    await _heartbeat(ctx, "testreport_fill: waiting for session lock")
    async with session._lock:
        path = _resolve_testreport_path(session)
        content = path.read_text(encoding="utf-8", errors="replace")
        lines = content.splitlines(keepends=True)
        counts = {"summary": 0, "reproducer": 0, "status": 0}
        for i, line in enumerate(lines):
            nl = "\n" if line.endswith("\n") else ""
            body = line[:-1] if nl else line
            if summary is not None:
                m = _SUMMARY_PLACEHOLDER.match(body)
                if m:
                    lines[i] = f"{m.group('pre')}{summary}{nl}"
                    counts["summary"] += 1
                    continue
            if reproducer is not None:
                m = _REPRODUCER_PLACEHOLDER.match(body)
                if m:
                    lines[i] = f"{m.group('pre')}{reproducer}{nl}"
                    counts["reproducer"] += 1
                    continue
            if status is not None:
                m = _STATUS_PLACEHOLDER.match(body)
                if m:
                    lines[i] = f"{m.group('pre')}{status}{nl}"
                    counts["status"] += 1
                    continue
        new_text = "".join(lines)
        bytes_written = _atomic_write_text(path, new_text)

    return {
        "path": str(path),
        "filled": counts,
        "bytes_written": bytes_written,
        "line_count": _count_lines(new_text),
    }


# --------------------------------------------------------------------------- #
# Registration                                                                #
# --------------------------------------------------------------------------- #


def register_testreport_tools(mcp: FastMCP, provider: SessionProvider) -> list[str]:
    """Register the testreport tools on ``mcp``.

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

    async def _read(
        offset: int = 1,
        limit: int | None = None,
        workspace: str = WORKSPACE_DEFAULT,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx, workspace)
        return await testreport_read(session, offset=offset, limit=limit, ctx=ctx)

    async def _logs(
        workspace: str = WORKSPACE_DEFAULT, ctx: Context | None = None
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx, workspace)
        return await testreport_logs(session, ctx=ctx)

    async def _read_file(
        relpath: str,
        workspace: str = WORKSPACE_DEFAULT,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx, workspace)
        return await testreport_read_file(session, relpath, ctx=ctx)

    async def _patch(
        start_line: int,
        end_line: int,
        replacement: str,
        workspace: str = WORKSPACE_DEFAULT,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx, workspace)
        return await testreport_patch(
            session, start_line, end_line, replacement, ctx=ctx
        )

    async def _write(
        content: str,
        workspace: str = WORKSPACE_DEFAULT,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx, workspace)
        return await testreport_write(session, content, ctx=ctx)

    async def _fill(
        reproducer: str | None = None,
        status: str | None = None,
        summary: str | None = None,
        ctx: Context | None = None,
    ) -> dict[str, Any]:
        session = await resolve_session(provider, ctx)
        return await testreport_fill(
            session, reproducer=reproducer, status=status, summary=summary, ctx=ctx
        )

    read_desc = (
        "Read the currently loaded testreport file. Returns the path, total "
        "line count, and content (utf-8, errors replaced). By default returns "
        "the whole file; pass `offset` (1-based first line) and/or `limit` (max "
        "lines) to read a line window instead — use this to page a large report "
        "(a Product Increment log can be thousands of lines) without overflowing. "
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
    logs_desc = (
        "List the auxiliary log files in the loaded testreport's checkout: the "
        "per-package/arch build-check logs (build_checks/) and the per-refhost "
        "install logs (install_logs/). Returns each file's name and size; fetch "
        "one with testreport_read_file."
    )
    read_file_desc = (
        "Read a file from the loaded testreport's checkout by relative path, "
        "e.g. 'build_checks/<pkg>.<arch>.log', 'install_logs/<host>.log', "
        "'source.diff' or 'patchinfo.xml'. The path may not escape the checkout "
        "directory. Returns path, line count and full content."
    )
    fill_desc = (
        "Bulk-set the repetitive per-bug placeholder tokens an exported "
        "testreport ships with, in one atomic write. `reproducer` (YES/NO) sets "
        "every unfilled `REPRODUCER_PRESENT:` line; `status` (one of FIXED, "
        "NOT_FIXED, HYPOTHETICAL, NOT_REPRODUCIBLE, NO_ENVIRONMENT, TOO_COMPLEX, "
        "SKIPPED, OTHER) sets every unfilled templated `STATUS:` line; `summary` "
        "(PASSED/FAILED) sets the top `SUMMARY:` line. Only exact template "
        "placeholders are touched, so it is idempotent and never overwrites a "
        "value you already set by hand. Ideal for CVE-heavy updates: call with "
        "reproducer=NO, status=SKIPPED (security policy), then override any "
        "non-security bug individually with testreport_patch. Returns a `filled` "
        "count per token. Still fill regression/build-log/source sections yourself."
    )

    specs = (
        (
            "testreport_read",
            _read,
            read_desc,
            ToolAnnotations(readOnlyHint=True, idempotentHint=True),
        ),
        (
            "testreport_logs",
            _logs,
            logs_desc,
            ToolAnnotations(readOnlyHint=True, idempotentHint=True),
        ),
        (
            "testreport_read_file",
            _read_file,
            read_file_desc,
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
        (
            "testreport_fill",
            _fill,
            fill_desc,
            ToolAnnotations(idempotentHint=True),
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
