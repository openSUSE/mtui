"""MCP <-> REPL parity: the two argv paths must agree.

mtui exposes every command two ways:

* the interactive REPL splits a shell line with :func:`shlex.split` and feeds
  the tokens to the command's :class:`argparse.ArgumentParser`;
* the MCP server receives ``{dest: value}`` kwargs, turns them back into an
  argv token list with :func:`mtui.mcp._argv.kwargs_to_argv`, and feeds *the
  same* parser.

PARITY INVARIANT: for equivalent inputs both paths must produce the same
parsed :class:`argparse.Namespace` **and** the same scope/template
resolution. This module locks that invariant with a reusable parity
assertion parametrised across the real command parsers, plus dedicated
regression cases for two argv-marshalling bugs that broke it:

* BUG1 (:mod:`mtui.mcp._argv`): an ``append`` action with
  ``nargs=REMAINDER`` (``commit -m``, ``lock -c``) used to be emitted into
  the flag list; a flag declared *after* it (``-T/--template``, added by
  ``_add_template_arg``) was then swallowed by the REMAINDER, so the message
  absorbed ``--template SUSE:...`` and ``template`` stayed ``None`` (wrong
  fan-out). The fix routes append+REMAINDER into the positional tail.
* BUG2 (:mod:`mtui.mcp.session`): ``start_jobs`` scoped a fanned-out job with
  ``[*argv, "-T", rrid]``; for a positional-REMAINDER command (``run``) the
  trailing ``-T rrid`` was swallowed into ``command`` and ``template`` stayed
  ``None``. The fix prepends the scope flag: ``["-T", rrid, *argv]``.

Assertions compare real :class:`argparse.Namespace` objects and real schema
dicts; nothing here mocks :func:`kwargs_to_argv` or the parsers.
"""

from __future__ import annotations

import json
import shlex
import sys
from argparse import REMAINDER, Namespace
from typing import TYPE_CHECKING, Any, ClassVar

import pytest

pytest.importorskip("mcp")

from mtui.commands import Command  # noqa: E402
from mtui.mcp._argv import kwargs_to_argv  # noqa: E402
from mtui.mcp._schema import build_parameters  # noqa: E402
from mtui.mcp.session import McpSession  # noqa: E402
from mtui.types.updateid import UpdateID  # noqa: E402

if TYPE_CHECKING:
    from pathlib import Path


# --------------------------------------------------------------------------- #
# Parity helpers                                                              #
# --------------------------------------------------------------------------- #


def _parser(command: str, *, prime: bool = False):
    """Build a FRESH parser for ``command``.

    ``argparse.parse_args`` mutates the namespace it is given and some parsers
    memoise per-parse state, so every parse in a parity check uses its own
    parser instance. ``prime=True`` runs :func:`build_parameters` first, which
    is required for mutex/synthetic-routing commands (``load_template``,
    ``set_repo``) because it attaches ``parser._mtui_synthetic_dests`` that
    :func:`kwargs_to_argv` consults.
    """
    parser = Command.registry[command].argparser(sys)
    if prime:
        build_parameters(parser)
    return parser


def _normalise(ns: Namespace) -> dict[str, Any]:
    """Turn a Namespace into a comparable dict.

    Most values are plain scalars/lists and compare directly. ``UpdateID``
    subclasses (``load_template``'s ``update`` dest) do not implement
    ``__eq__``, so two equivalent instances would compare unequal by identity;
    represent them by ``(class name, str(id))`` which is exactly what parity
    cares about.
    """
    out: dict[str, Any] = {}
    for key, value in vars(ns).items():
        if isinstance(value, UpdateID):
            out[key] = (type(value).__name__, str(value.id))
        else:
            out[key] = value
    return out


def _assert_parity(
    command: str,
    repl_line: str,
    kwargs: dict[str, Any],
    *,
    prime: bool = False,
) -> Namespace:
    """Assert the MCP and REPL argv paths parse to the same Namespace.

    Builds ``command``'s real parser, encodes ``kwargs`` via
    :func:`kwargs_to_argv`, and parses that argv; separately parses
    ``shlex.split(repl_line)``. The two namespaces must be equal. Returns the
    MCP-path namespace so callers can make extra assertions on it.
    """
    argv = kwargs_to_argv(_parser(command, prime=prime), kwargs)
    mcp_ns = _parser(command).parse_args(argv)
    repl_ns = _parser(command).parse_args(shlex.split(repl_line))
    assert _normalise(mcp_ns) == _normalise(repl_ns), (
        f"parity broke for {command!r}: "
        f"MCP argv {argv} -> {vars(mcp_ns)} != REPL {vars(repl_ns)}"
    )
    return mcp_ns


# --------------------------------------------------------------------------- #
# Parity matrix (design phase-1 cases that must hold on the fixed code)       #
# --------------------------------------------------------------------------- #
#
# Each row: (id, command, repl_line, kwargs, prime_schema). GAP-A rows
# (empty-list optional-REMAINDER, e.g. reject with message=[]) are excluded:
# that divergence is a separate, out-of-scope issue and is not one of the two
# fixes locked here.

_MATRIX: list[tuple[str, str, str, dict[str, Any], bool]] = [
    ("store_true", "add_host", "--keep-mode", {"keep_mode": True, "target": []}, False),
    (
        "store_const",
        "update",
        "--noprepare",
        {"noprepare": True, "newpackage": False, "hosts": []},
        False,
    ),
    (
        "append_plain",
        "add_host",
        "--target h1 --target h2",
        {"target": ["h1", "h2"], "keep_mode": False},
        False,
    ),
    (
        "append_optional_nargs",
        "reject",
        "-g qam -g sec -r admin",
        {"reason": "admin", "group": ["qam", "sec"], "user": ""},
        False,
    ),
    ("positional_scalar", "put", "payload.bin", {"filename": "payload.bin"}, False),
    ("positional_plus", "install", "vim nano", {"package": ["vim", "nano"]}, False),
    (
        "remainder_positional_flag",
        "run",
        "--target h1 ls -la",
        {"command": ["ls", "-la"], "hosts": ["h1"]},
        False,
    ),
    (
        "remainder_positional_template",
        "run",
        "-T SUSE:Maintenance:1:1 ls -la",
        {"command": ["ls", "-la"], "hosts": [], "template": "SUSE:Maintenance:1:1"},
        False,
    ),
    (
        "append_remainder_with_template_commit",  # BUG1
        "commit",
        "-T SUSE:Maintenance:1:1 -m fix bug",
        {"msg": ["fix", "bug"], "template": "SUSE:Maintenance:1:1"},
        False,
    ),
    (
        "append_remainder_with_template_lock",  # BUG1
        "lock",
        "-T SUSE:Maintenance:1:1 -c busy",
        {"comment": ["busy"], "template": "SUSE:Maintenance:1:1", "hosts": []},
        False,
    ),
    (
        # Adversarial: a message token that itself looks like a flag
        # (``--template``) must survive the REMAINDER verbatim on both paths and
        # not be re-parsed as the command's own ``--template`` option.
        "append_remainder_flaglike_token",  # BUG1
        "commit",
        "-T SUSE:Maintenance:1:1 -m --template x",
        {"msg": ["--template", "x"], "template": "SUSE:Maintenance:1:1"},
        False,
    ),
    (
        "optional_remainder_template_first",
        "reject",
        "-T SUSE:Maintenance:1:1 -g qam -r admin -m bad update",
        {
            "reason": "admin",
            "group": ["qam"],
            "user": "",
            "message": ["bad", "update"],
            "template": "SUSE:Maintenance:1:1",
        },
        False,
    ),
    (
        "mutex_synthetic_route",
        "load_template",
        "-k SUSE:Maintenance:1:1",
        {"kernel_review_id": "SUSE:Maintenance:1:1", "auto_review_id": None},
        True,
    ),
    (
        "mutex_storeconst_enum",
        "set_repo",
        "--remove",
        {"operation": "remove", "hosts": []},
        True,
    ),
]


@pytest.mark.parametrize(
    ("command", "repl_line", "kwargs", "prime"),
    [(c, r, k, p) for _id, c, r, k, p in _MATRIX],
    ids=[row[0] for row in _MATRIX],
)
def test_argv_repl_parity(
    command: str,
    repl_line: str,
    kwargs: dict[str, Any],
    prime: bool,
) -> None:
    """Every matrix case: MCP kwargs and REPL line parse to the same Namespace."""
    _assert_parity(command, repl_line, kwargs, prime=prime)


# --------------------------------------------------------------------------- #
# BUG1 regressions: append+REMAINDER must not swallow a later --template      #
# --------------------------------------------------------------------------- #


def test_commit_message_with_template_not_swallowed() -> None:
    """``commit -m`` (append+REMAINDER) keeps a later ``--template`` a real flag.

    Regression for BUG1: the append+REMAINDER encoder emitted ``-m`` into the
    flag list, so ``--template`` (declared after ``-m`` by ``_add_template_arg``)
    was appended behind it and swallowed by the REMAINDER, corrupting the
    message and leaving ``template=None`` (wrong fan-out). The fix routes the
    append+REMAINDER into the positional tail so ``--template`` precedes it.
    """
    parser = _parser("commit")
    argv = kwargs_to_argv(parser, {"msg": ["fix", "bug"], "template": "SUSE:M:1:1"})
    # --template must be emitted BEFORE the REMAINDER message tokens.
    assert argv == ["--template", "SUSE:M:1:1", "--msg", "fix", "bug"]

    parsed = _parser("commit").parse_args(argv)
    assert parsed.msg == [["fix", "bug"]]
    assert " ".join(parsed.msg[0]) == "fix bug"
    assert parsed.template == "SUSE:M:1:1"


def test_lock_comment_with_template_not_swallowed() -> None:
    """``lock -c`` (append+REMAINDER) keeps a later ``--template`` intact.

    Same BUG1 shape as ``commit``: ``hostslock`` adds ``-c`` then
    ``_add_template_arg``.
    """
    parser = _parser("lock")
    argv = kwargs_to_argv(
        parser,
        {"comment": ["busy"], "template": "SUSE:M:2:1", "hosts": []},
    )
    assert argv == ["--template", "SUSE:M:2:1", "--comment", "busy"]

    parsed = _parser("lock").parse_args(argv)
    assert parsed.comment == [["busy"]]
    assert parsed.template == "SUSE:M:2:1"


# --------------------------------------------------------------------------- #
# BUG2 regressions: start_jobs must scope with -T FIRST, not trailing         #
# --------------------------------------------------------------------------- #


def _make_session(tmp_path: Path) -> McpSession:
    import logging
    from unittest.mock import MagicMock

    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return McpSession(cfg, logging.getLogger("test.mcp.parity"))


def _load_two_reports(sess: McpSession) -> tuple[str, str]:
    """Load two templates so a fan-out command resolves to both."""
    rrids = ("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1")
    from unittest.mock import MagicMock

    for rrid in rrids:
        report = MagicMock()
        report.id = rrid
        report.targets = {}
        sess.templates.add(report)
    sess.templates.set_active(rrids[0])
    return rrids


def _capture_probe(name: str, nargs: object) -> type[Command]:
    """A throwaway fan-out command with a positional of the given ``nargs``.

    Its body records the parsed ``command`` positional and ``template`` as a
    JSON line so the test can read exactly what argparse produced from the
    scoped argv ``start_jobs`` built. Defining the class registers it in
    ``Command.registry`` under ``name``; the caller unregisters it.
    """

    class _Probe(Command):
        command = name
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:  # noqa: ANN001
            parser.add_argument("command", nargs=nargs)
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            self.println(
                json.dumps(
                    {
                        "command": list(self.args.command),
                        "template": self.args.template,
                    }
                )
            )

    return _Probe


def _run_fanout(sess: McpSession, cls: type[Command], argv: list[str]) -> list[dict]:
    import asyncio

    async def driver() -> list[str]:
        ids = await sess.start_jobs(cls, argv)
        for jid in ids:
            await sess._jobs[jid]["task"]  # noqa: SLF001 - wait for workers
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)
    # Each correctly-scoped job emits exactly ONE JSON line. A mis-scoped job
    # (template swallowed -> template=None -> the body re-fans-out across every
    # template) emits several; parse the LAST line so the assertions on
    # ``command``/``template`` fail cleanly instead of a JSON decode crash.
    rows: list[dict] = []
    for jid in ids:
        raw = sess.job_result(jid)
        lines = [ln for ln in raw.splitlines() if ln.strip()]
        assert lines, f"job {jid} produced no output; scope resolution broke: {raw!r}"
        try:
            rows.append(json.loads(lines[-1]))
        except json.JSONDecodeError:  # pragma: no cover - only on a broken scope
            pytest.fail(f"job {jid} output was not one JSON line; got {raw!r}")
    return rows


def test_start_jobs_scopes_remainder_command_without_swallowing_flag(
    tmp_path: Path,
) -> None:
    """BUG2: a fanned-out REMAINDER command (``run``-shaped) keeps ``-T`` a flag.

    ``start_jobs`` prepends ``-T <rrid>``; the positional ``nargs=REMAINDER``
    ``command`` must stay ``['ls', '-la']`` (no ``-T`` leaked in) and each job's
    ``template`` must be its own rrid. With the pre-fix ``[*argv, '-T', rrid]``
    ordering the REMAINDER swallowed ``-T <rrid>`` -> ``command`` gained
    ``['-T', 'SUSE:...']`` and ``template`` stayed ``None``.
    """
    sess = _make_session(tmp_path)
    rrid1, rrid2 = _load_two_reports(sess)
    cls = _capture_probe("run_like_probe_tmp", REMAINDER)

    captured = _run_fanout(sess, cls, ["ls", "-la"])

    assert len(captured) == 2
    for row in captured:
        assert row["command"] == ["ls", "-la"], row
        assert row["template"] in {rrid1, rrid2}, row
        assert "-T" not in row["command"]
    assert {row["template"] for row in captured} == {rrid1, rrid2}


def test_start_jobs_scopes_plus_command(tmp_path: Path) -> None:
    """A ``nargs='+'`` SLOW_COMMAND (``install``-shaped) scopes per template.

    This is NOT a BUG2 regression: a ``+`` positional (unlike REMAINDER) still
    recognises registered optionals, so it parses correctly for either ``-T``
    ordering (it passes even on the pre-fix code). It locks the ``+``-scoping
    contract — the prepended ``-T`` keeps the package list intact and scopes
    each job to its own template — complementing the REMAINDER regression above.
    """
    from mtui.mcp.tools import SLOW_COMMANDS

    assert "install" in SLOW_COMMANDS  # the real nargs='+' slow command

    sess = _make_session(tmp_path)
    rrid1, rrid2 = _load_two_reports(sess)
    cls = _capture_probe("install_like_probe_tmp", "+")

    captured = _run_fanout(sess, cls, ["vim", "nano"])

    assert len(captured) == 2
    for row in captured:
        assert row["command"] == ["vim", "nano"], row
        assert row["template"] in {rrid1, rrid2}, row
    assert {row["template"] for row in captured} == {rrid1, rrid2}


# --------------------------------------------------------------------------- #
# Safety / parity invariant: a smuggled --force cannot reach approve/reject   #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize("command", ["approve", "reject"])
def test_force_absent_from_schema_and_inert_when_smuggled(command: str) -> None:
    """``--force`` is not an ``approve``/``reject`` parameter and can't be forged.

    Neither command declares ``--force`` (only ``assign`` does), so it must be
    absent from the built MCP parameter schema. And a client that smuggles a
    ``force`` kwarg gets it dropped: ``kwargs_to_argv`` only emits tokens for
    declared actions, so no ``--force``/``-f`` reaches argparse and the parsed
    namespace has no ``force`` attribute — inert regardless of interactivity.
    """
    parser = _parser(command)
    params = {p.name for p in build_parameters(parser)}
    assert "force" not in params

    argv = kwargs_to_argv(
        _parser(command),
        {"reason": "admin", "group": [], "user": "", "force": True},
    )
    assert "--force" not in argv
    assert "-f" not in argv

    parsed = _parser(command).parse_args(argv)
    assert not hasattr(parsed, "force")
