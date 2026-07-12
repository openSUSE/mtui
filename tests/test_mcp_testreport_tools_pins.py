"""Mutation-killing pins for :mod:`mtui.mcp.testreport_tools`.

A full mutmut run left survivors in code paths the existing suite
executes but never asserts on (``tests/test_mcp_testreport_tools.py``
holds the behavioural baseline). Each test here pins one observable
contract a surviving mutant silently broke:

* ``testreport_fill`` output is asserted with **whole-file equality**,
  so a dropped trailing newline (the ``line.endswith("\\n")`` mutants)
  can no longer glue consecutive fields together unnoticed; the
  ``YES``/``FAILED`` halves of the accepted-value tuples are exercised.
* ``testreport_read``: ``limit=0`` is a legal empty window, the
  windowed reply carries ``path``, ``relpath=`` honours ``template=``
  scoping, invalid utf-8 decodes with replacement characters, and the
  ``[mcp] max_output_bytes`` budget really truncates.
* ``testreport_patch``: the file's **last** line (and appending after
  it) is in bounds, and ``path``/``bytes_written`` round-trip.
* ``testreport_logs``: heartbeat frame, ``size``/``path`` payload, and
  ``template=`` scoping.
* the uniform :class:`McpCommandError` envelope promised by the module
  docstring — empty stdout, one-sentence stderr, ``exit_code=1``.
* ``_atomic_write_text`` builds its tempfile as a hidden sibling of the
  destination (``dir=path.parent``, ``.{name}.``/``.tmp`` affixes).
* every tool passes the resolved ``template`` rrid to
  ``session.scoped_lock`` (per-template, not global, serialisation).

Fixtures mirror ``tests/test_mcp_testreport_tools.py`` (SimpleNamespace
sessions driven through :func:`asyncio.run`) but live locally so no
existing test file is touched; the ``scoped_lock`` fake here
additionally records the rrid it was asked for.
"""

from __future__ import annotations

import asyncio
import logging
import tempfile
from collections.abc import Mapping
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.mcp import testreport_tools as tt
from mtui.mcp.session import McpCommandError, McpSession

RRID_A = "SUSE:Maintenance:1:1"
RRID_B = "SUSE:Maintenance:2:2"

# --------------------------------------------------------------------------- #
# Fixtures                                                                    #
# --------------------------------------------------------------------------- #


class _FakeRegistry:
    """Minimal stand-in for the template registry surface the tools read."""

    def __init__(self, reports: Mapping[str, object]) -> None:
        self._entries = dict(reports)

    def get(self, rrid: str) -> object:
        return self._entries[rrid]

    def all(self) -> list[object]:
        return list(self._entries.values())

    @property
    def active(self) -> object:
        return next(iter(self._entries.values()))


def _session(reports: Mapping[str, Path], cap: int | None = None) -> SimpleNamespace:
    """Fake session over ``{rrid: log path}`` with a recording scoped_lock.

    Unlike the fake in ``tests/test_mcp_testreport_tools.py`` the
    ``scoped_lock`` stand-in records every rrid it is asked for in
    ``sess.lock_calls`` so tests can pin per-template lock scoping.
    """
    entries = {
        rrid: SimpleNamespace(id=rrid, path=str(p)) for rrid, p in reports.items()
    }
    lock = asyncio.Lock()
    lock_calls: list[str | None] = []

    @asynccontextmanager
    async def _scoped_lock(rrid: str | None):
        lock_calls.append(rrid)
        async with lock:
            yield

    sess = SimpleNamespace(
        metadata=next(iter(entries.values())),
        templates=_FakeRegistry(entries),
        scoped_lock=_scoped_lock,
        lock_calls=lock_calls,
    )
    if cap is not None:
        sess.config = SimpleNamespace(mcp_max_output_bytes=cap)
    return sess


def _null_session(tmp_path: Path) -> McpSession:
    """Real :class:`McpSession` whose metadata is the default NullTestReport."""
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return McpSession(cfg, logging.getLogger("test.mcp.testreport.pins"))


def _two_checkouts(tmp_path: Path) -> dict[str, Path]:
    """Two template checkout dirs, each with its own ``log`` file."""
    dirs: dict[str, Path] = {}
    for name, rrid in (("a", RRID_A), ("b", RRID_B)):
        d = tmp_path / name
        d.mkdir()
        (d / "log").write_text(f"{name}-log\n", encoding="utf-8")
        dirs[rrid] = d
    return dirs


class _RecordingCtx:
    """Context fake whose ``report_progress`` requires every keyword.

    All three parameters are keyword-only **without defaults** so a
    mutant that drops one of the call's keyword arguments raises
    ``TypeError`` inside ``_heartbeat`` (which swallows it) and the
    frame never lands in ``calls``.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[float | None, float | None, str | None]] = []

    async def report_progress(
        self,
        *,
        progress: float | None,
        total: float | None,
        message: str | None,
    ) -> None:
        self.calls.append((progress, total, message))


# --------------------------------------------------------------------------- #
# testreport_fill — whole-file rendering and accepted values                  #
# --------------------------------------------------------------------------- #

# The final STATUS line deliberately has no trailing newline so both halves of
# the ``nl = "\n" if line.endswith("\n") else ""`` branch are rendered.
_FILL_INPUT = (
    "SUMMARY: PASSED/FAILED\n"
    "\n"
    'bnc#1001 ("first bug"):\n'
    "REPRODUCER_PRESENT: YES/NO\n"
    "STATUS:             FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/"
    "NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER\n"
    "\n"
    'bnc#1002 ("second bug"):\n'
    "REPRODUCER_PRESENT: NO\n"  # hand-filled -> must stay untouched
    "STATUS:             FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/"
    "NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER"  # no trailing newline
)

_FILL_EXPECTED = (
    "SUMMARY: FAILED\n"
    "\n"
    'bnc#1001 ("first bug"):\n'
    "REPRODUCER_PRESENT: YES\n"
    "STATUS:             NOT_FIXED\n"
    "\n"
    'bnc#1002 ("second bug"):\n'
    "REPRODUCER_PRESENT: NO\n"
    "STATUS:             NOT_FIXED"
)


def test_fill_renders_the_exact_expected_file(tmp_path: Path) -> None:
    """Whole-file equality: newlines survive and YES/FAILED are accepted."""
    file = tmp_path / "log"
    file.write_text(_FILL_INPUT, encoding="utf-8")
    sess = _session({RRID_A: file})

    res = asyncio.run(
        tt.testreport_fill(
            sess,  # ty: ignore[invalid-argument-type]
            reproducer="YES",
            status="NOT_FIXED",
            summary="FAILED",
        )
    )

    assert file.read_text(encoding="utf-8") == _FILL_EXPECTED
    assert res == {
        "path": str(file),
        "filled": {"summary": 1, "reproducer": 1, "status": 2},
        "bytes_written": len(_FILL_EXPECTED.encode("utf-8")),
        "line_count": 9,
    }


def test_fill_with_template_targets_only_that_file(tmp_path: Path) -> None:
    """``template=<rrid>`` fills exactly that checkout's log file."""
    dirs = _two_checkouts(tmp_path)
    for d in dirs.values():
        (d / "log").write_text("SUMMARY: PASSED/FAILED\n", encoding="utf-8")
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    res = asyncio.run(
        tt.testreport_fill(sess, summary="PASSED", template=RRID_B)  # ty: ignore[invalid-argument-type]
    )

    assert (dirs[RRID_B] / "log").read_text(encoding="utf-8") == "SUMMARY: PASSED\n"
    # The sibling template stays untouched.
    assert (dirs[RRID_A] / "log").read_text(encoding="utf-8") == (
        "SUMMARY: PASSED/FAILED\n"
    )
    assert res["path"] == str(dirs[RRID_B] / "log")
    assert res["filled"] == {"summary": 1, "reproducer": 0, "status": 0}


@pytest.mark.parametrize(
    ("kwargs", "stderr"),
    [
        ({"reproducer": "MAYBE"}, "reproducer must be YES or NO, got 'MAYBE'"),
        (
            {"status": "DONE"},
            "status must be one of ['FIXED', 'HYPOTHETICAL', 'NOT_FIXED', "
            "'NOT_REPRODUCIBLE', 'NO_ENVIRONMENT', 'OTHER', 'SKIPPED', "
            "'TOO_COMPLEX'], got 'DONE'",
        ),
        ({"summary": "OK"}, "summary must be PASSED or FAILED, got 'OK'"),
        ({}, "nothing to fill: pass at least one of reproducer/status/summary"),
    ],
)
def test_fill_validation_error_envelope(
    kwargs: dict[str, str], stderr: str, tmp_path: Path
) -> None:
    """Each rejected fill carries the uniform empty-stdout/exit-1 envelope."""
    file = tmp_path / "log"
    file.write_text(_FILL_INPUT, encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_fill(sess, **kwargs))  # ty: ignore[invalid-argument-type]

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == ("", stderr, 1)
    # Validation must reject before touching the file.
    assert file.read_text(encoding="utf-8") == _FILL_INPUT


# --------------------------------------------------------------------------- #
# Heartbeat: one exact progress frame per tool                                #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    ("tool_name", "invoke"),
    [
        ("testreport_read", lambda s, c: tt.testreport_read(s, ctx=c)),
        ("testreport_logs", lambda s, c: tt.testreport_logs(s, ctx=c)),
        ("testreport_patch", lambda s, c: tt.testreport_patch(s, 1, 1, "X\n", ctx=c)),
        ("testreport_write", lambda s, c: tt.testreport_write(s, "new\n", ctx=c)),
        ("testreport_fill", lambda s, c: tt.testreport_fill(s, reproducer="NO", ctx=c)),
    ],
)
def test_every_tool_emits_one_named_heartbeat_frame(
    tool_name: str, invoke: Any, tmp_path: Path
) -> None:
    """Each tool emits exactly one ``progress=0.0`` frame naming itself."""
    file = tmp_path / "log"
    file.write_text("x\n", encoding="utf-8")
    sess = _session({RRID_A: file})
    ctx = _RecordingCtx()

    asyncio.run(invoke(sess, ctx))

    assert ctx.calls == [(0.0, None, f"{tool_name}: waiting for template lock")]


# --------------------------------------------------------------------------- #
# Per-template lock scoping                                                   #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "invoke",
    [
        lambda s: tt.testreport_read(s, template=RRID_B),
        lambda s: tt.testreport_read(s, relpath="log", template=RRID_B),
        lambda s: tt.testreport_logs(s, template=RRID_B),
        lambda s: tt.testreport_patch(s, 1, 1, "X\n", template=RRID_B),
        lambda s: tt.testreport_write(s, "new\n", template=RRID_B),
        lambda s: tt.testreport_fill(s, reproducer="NO", template=RRID_B),
    ],
)
def test_every_tool_requests_the_scoped_templates_lock(
    invoke: Any, tmp_path: Path
) -> None:
    """``session.scoped_lock`` receives the requested rrid, not ``None``."""
    dirs = _two_checkouts(tmp_path)
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    asyncio.run(invoke(sess))

    assert sess.lock_calls == [RRID_B]


# --------------------------------------------------------------------------- #
# testreport_read — windows, cap, decoding, scoping                           #
# --------------------------------------------------------------------------- #


def test_read_limit_zero_is_a_legal_empty_window(tmp_path: Path) -> None:
    """``limit=0`` returns an empty window (no error) with the full shape."""
    file = tmp_path / "log"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_read(sess, limit=0))  # ty: ignore[invalid-argument-type]

    assert result == {
        "path": str(file),
        "line_count": 3,
        "offset": 1,
        "returned_lines": 0,
        "content": "",
    }


def test_read_window_reply_shape_is_complete(tmp_path: Path) -> None:
    """The windowed reply carries path/line_count/offset/returned_lines."""
    file = tmp_path / "log"
    file.write_text("a\nb\nc\nd\ne\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_read(sess, offset=2, limit=2))  # ty: ignore[invalid-argument-type]

    assert result == {
        "path": str(file),
        "line_count": 5,
        "offset": 2,
        "returned_lines": 2,
        "content": "b\nc\n",
    }


@pytest.mark.parametrize(
    ("kwargs", "stderr"),
    [
        ({"offset": 0}, "offset must be >= 1 (got 0)"),
        ({"offset": -3}, "offset must be >= 1 (got -3)"),
        ({"limit": -1}, "limit must be >= 0 (got -1)"),
    ],
)
def test_read_window_validation_error_envelope(
    kwargs: dict[str, int], stderr: str, tmp_path: Path
) -> None:
    """Rejected windows carry the uniform empty-stdout/exit-1 envelope."""
    file = tmp_path / "log"
    file.write_text("a\nb\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_read(sess, **kwargs))  # ty: ignore[invalid-argument-type]

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == ("", stderr, 1)


def test_read_relpath_is_scoped_to_the_requested_template(tmp_path: Path) -> None:
    """``relpath=`` + ``template=`` reads that template's own aux file."""
    dirs = _two_checkouts(tmp_path)
    for name, rrid in (("alpha", RRID_A), ("beta", RRID_B)):
        sub = dirs[rrid] / "build_checks"
        sub.mkdir()
        (sub / "pkg.log").write_text(f"{name}\n", encoding="utf-8")
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    result = asyncio.run(
        tt.testreport_read(
            sess,  # ty: ignore[invalid-argument-type]
            relpath="build_checks/pkg.log",
            template=RRID_B,
        )
    )

    assert result["path"] == str(dirs[RRID_B] / "build_checks" / "pkg.log")
    assert result["content"] == "beta\n"


def test_read_replaces_invalid_utf8_bytes(tmp_path: Path) -> None:
    """Undecodable bytes come back as U+FFFD instead of raising."""
    file = tmp_path / "log"
    file.write_bytes(b"ok\n\xff\xfe raw\n")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]

    assert result["content"] == "ok\n�� raw\n"
    assert result["line_count"] == 2


def test_read_honours_the_session_output_cap(tmp_path: Path) -> None:
    """``[mcp] max_output_bytes`` truncates both full and windowed reads."""
    file = tmp_path / "log"
    body = ("a" * 40) + "\n" + ("b" * 40) + "\n"
    file.write_text(body, encoding="utf-8")
    sess = _session({RRID_A: file}, cap=16)

    full = asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]
    assert full["content"].startswith("a" * 16)
    assert "a" * 17 not in full["content"]
    assert "truncated" in full["content"]

    window = asyncio.run(tt.testreport_read(sess, offset=2))  # ty: ignore[invalid-argument-type]
    assert window["content"].startswith("b" * 16)
    assert "b" * 17 not in window["content"]
    assert "truncated" in window["content"]


def test_output_cap_defaults_to_zero_without_config() -> None:
    """A session without a populated config disables the cap with 0."""
    assert tt._output_cap(SimpleNamespace()) == 0  # ty: ignore[invalid-argument-type]


def test_read_missing_relpath_error_envelope(tmp_path: Path) -> None:
    """A missing checkout file is refused with the uniform envelope."""
    file = tmp_path / "log"
    file.write_text("x\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(
            tt.testreport_read(sess, relpath="build_checks/nope.log")  # ty: ignore[invalid-argument-type]
        )

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        "no such file in testreport checkout: build_checks/nope.log",
        1,
    )


@pytest.mark.parametrize("relpath", ["../secret.txt", "/etc/passwd"])
def test_read_traversal_refusal_envelope(relpath: str, tmp_path: Path) -> None:
    """Escaping relpaths are refused with the uniform envelope."""
    file = tmp_path / "log"
    file.write_text("x\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_read(sess, relpath=relpath))  # ty: ignore[invalid-argument-type]

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        f"path {relpath!r} escapes the testreport directory",
        1,
    )


# --------------------------------------------------------------------------- #
# testreport_patch — boundaries, reply shape, decoding                        #
# --------------------------------------------------------------------------- #


def test_patch_can_replace_the_last_line(tmp_path: Path) -> None:
    """``start_line == end_line == n`` is in bounds on an n-line file."""
    file = tmp_path / "log"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_patch(sess, 3, 3, "C\n"))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nb\nC\n"
    assert result == {
        "path": str(file),
        "new_line_count": 3,
        "replaced_lines": 1,
        "inserted_lines": 1,
        "bytes_written": len(b"a\nb\nC\n"),
    }


def test_patch_can_append_after_the_last_line(tmp_path: Path) -> None:
    """``start_line=n+1, end_line=n`` is a pure insertion at EOF."""
    file = tmp_path / "log"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_patch(sess, 4, 3, "d\n"))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == "a\nb\nc\nd\n"
    assert result["replaced_lines"] == 0
    assert result["inserted_lines"] == 1
    assert result["new_line_count"] == 4


def test_patch_out_of_bounds_error_envelope(tmp_path: Path) -> None:
    """The refusal names the requested range and the actual line count."""
    file = tmp_path / "log"
    file.write_text("a\nb\nc\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_patch(sess, 10, 12, "x\n"))  # ty: ignore[invalid-argument-type]

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        "line range out of bounds: start_line=10, end_line=12, file has 3 line(s)",
        1,
    )
    # The refusal must not touch the file.
    assert file.read_text(encoding="utf-8") == "a\nb\nc\n"


def test_patch_tolerates_invalid_utf8_in_the_existing_file(tmp_path: Path) -> None:
    """A patch on a file with undecodable bytes replaces them, not raises."""
    file = tmp_path / "log"
    file.write_bytes(b"\xffheader\nold\n")
    sess = _session({RRID_A: file})

    asyncio.run(tt.testreport_patch(sess, 2, 2, "new\n"))  # ty: ignore[invalid-argument-type]

    assert file.read_bytes() == "�header\nnew\n".encode()


# --------------------------------------------------------------------------- #
# testreport_write — template scoping and reply shape                         #
# --------------------------------------------------------------------------- #


def test_write_with_template_targets_only_that_file(tmp_path: Path) -> None:
    """``template=<rrid>`` overwrites exactly that checkout's log file."""
    dirs = _two_checkouts(tmp_path)
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    result = asyncio.run(
        tt.testreport_write(sess, "rewritten\n", template=RRID_B)  # ty: ignore[invalid-argument-type]
    )

    assert (dirs[RRID_B] / "log").read_text(encoding="utf-8") == "rewritten\n"
    assert (dirs[RRID_A] / "log").read_text(encoding="utf-8") == "a-log\n"
    assert result == {
        "path": str(dirs[RRID_B] / "log"),
        "bytes_written": len(b"rewritten\n"),
        "line_count": 1,
    }


def test_write_empty_content_reports_zero_lines(tmp_path: Path) -> None:
    """Writing ``""`` truncates the file and reports 0 lines / 0 bytes."""
    file = tmp_path / "log"
    file.write_text("old\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    result = asyncio.run(tt.testreport_write(sess, ""))  # ty: ignore[invalid-argument-type]

    assert file.read_text(encoding="utf-8") == ""
    assert result == {"path": str(file), "bytes_written": 0, "line_count": 0}


# --------------------------------------------------------------------------- #
# testreport_logs — payload shape and template scoping                        #
# --------------------------------------------------------------------------- #


def test_logs_reports_names_sizes_and_checkout_path(tmp_path: Path) -> None:
    """The listing carries exact name/size pairs (sorted) and the base path."""
    (tmp_path / "build_checks").mkdir()
    (tmp_path / "build_checks" / "aaa.x86_64.log").write_text(
        "12345\n", encoding="utf-8"
    )
    (tmp_path / "build_checks" / "zzz.s390x.log").write_text(
        "1234567\n", encoding="utf-8"
    )
    (tmp_path / "install_logs").mkdir()
    (tmp_path / "install_logs" / "host1.log").write_text("abc\n", encoding="utf-8")
    file = tmp_path / "log"
    file.write_text("x\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    logs = asyncio.run(tt.testreport_logs(sess))  # ty: ignore[invalid-argument-type]

    assert logs == {
        "path": str(tmp_path),
        "build_checks": [
            {"name": "aaa.x86_64.log", "size": 6},
            {"name": "zzz.s390x.log", "size": 8},
        ],
        "install_logs": [{"name": "host1.log", "size": 4}],
    }


def test_logs_scoped_to_the_requested_template(tmp_path: Path) -> None:
    """``template=<rrid>`` lists that checkout's own aux logs only."""
    dirs = _two_checkouts(tmp_path)
    for name, rrid in (("alpha", RRID_A), ("beta", RRID_B)):
        sub = dirs[rrid] / "build_checks"
        sub.mkdir()
        (sub / f"{name}.log").write_text(f"{name}\n", encoding="utf-8")
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    logs = asyncio.run(tt.testreport_logs(sess, template=RRID_B))  # ty: ignore[invalid-argument-type]

    assert logs["path"] == str(dirs[RRID_B])
    assert [f["name"] for f in logs["build_checks"]] == ["beta.log"]
    assert logs["install_logs"] == []


# --------------------------------------------------------------------------- #
# _resolve_report / _resolve_testreport_path — exact refusal envelopes        #
# --------------------------------------------------------------------------- #


def test_unknown_template_error_envelope(tmp_path: Path) -> None:
    """An unloaded rrid is refused with the exact documented envelope."""
    file = tmp_path / "log"
    file.write_text("x\n", encoding="utf-8")
    sess = _session({RRID_A: file})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(
            tt.testreport_read(sess, template="SUSE:Maintenance:9:9")  # ty: ignore[invalid-argument-type]
        )

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        "template not loaded: SUSE:Maintenance:9:9",
        1,
    )


def test_multiple_templates_unscoped_error_envelope(tmp_path: Path) -> None:
    """The multi-template refusal joins the rrids with ', ' exactly."""
    dirs = _two_checkouts(tmp_path)
    sess = _session({rrid: d / "log" for rrid, d in dirs.items()})

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_read(sess))  # ty: ignore[invalid-argument-type]

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        f"multiple templates loaded ({RRID_A}, {RRID_B}); pass template=<rrid>",
        1,
    )


def test_no_testreport_loaded_error_envelope(tmp_path: Path) -> None:
    """The nothing-loaded refusal carries the exact documented envelope."""
    sess = _null_session(tmp_path)

    with pytest.raises(McpCommandError) as ei:
        asyncio.run(tt.testreport_read(sess))

    assert (ei.value.stdout, ei.value.stderr, ei.value.exit_code) == (
        "",
        "no testreport loaded; run `load_template` first",
        1,
    )


# --------------------------------------------------------------------------- #
# _atomic_write_text — sibling hidden tempfile                                #
# --------------------------------------------------------------------------- #


def test_atomic_write_builds_hidden_sibling_tempfile(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The tempfile lands next to the target with the ``.{name}.*.tmp`` shape.

    ``dir=path.parent`` is the crux of the atomic-write pattern: the
    temp file must live on the same filesystem as the destination or
    :func:`os.replace` stops being atomic (EXDEV). We record the
    kwargs of the real ``NamedTemporaryFile`` call to pin them.
    """
    recorded: dict[str, Any] = {}
    real_ntf = tempfile.NamedTemporaryFile

    def recording_ntf(*args: Any, **kwargs: Any) -> Any:
        recorded.update(kwargs)
        return real_ntf(*args, **kwargs)

    monkeypatch.setattr(tt.tempfile, "NamedTemporaryFile", recording_ntf)

    file = tmp_path / "report.txt"
    file.write_text("old\n", encoding="utf-8")

    assert tt._atomic_write_text(file, "new\n") == 4
    assert file.read_text(encoding="utf-8") == "new\n"

    assert recorded["dir"] == file.parent
    assert recorded["prefix"] == ".report.txt."
    assert recorded["suffix"] == ".tmp"
    assert recorded["delete"] is False
    assert recorded["mode"] == "wb"
    # No residue: the tempfile was swapped into place, not left behind.
    leftovers = [p.name for p in tmp_path.iterdir() if p.name != "report.txt"]
    assert leftovers == []
