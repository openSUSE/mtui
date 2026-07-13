"""Mutation-killing pin for ``build_tools``'s REPL_ONLY drift-check.

``build_tools`` opens with a defensive check: if ``mtui.mcp.deny.REPL_ONLY``
ever names a command no longer present in ``Command.registry`` (e.g. after
a rename), it should warn loudly at boot instead of silently building a
tool surface that has quietly dropped the guard. Because no *current*
deny-list entry has actually drifted, this branch never fires under the
existing suite, so a full mutmut run left its internals (the ``&``/``!=``/
``-`` operators) entirely unasserted.

This module simulates the drift by monkeypatching ``mtui.mcp.tools.REPL_ONLY``
with an extra, unregistered command name, and pins both directions:

* the normal (non-drifted) case emits no such warning;
* the drifted case emits exactly one warning naming the missing entry.
"""

from __future__ import annotations

import logging
from pathlib import Path
from unittest.mock import MagicMock

import pytest

pytest.importorskip("mcp")

from mcp.server.fastmcp import FastMCP  # noqa: E402

import mtui.mcp.tools as tools_mod  # noqa: E402
from mtui.mcp.session import McpSession  # noqa: E402


@pytest.fixture
def session(tmp_path: Path) -> McpSession:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return McpSession(cfg, logging.getLogger("test.mcp.tools.leftovers"))


def _warnings(records: list[logging.LogRecord]) -> list[logging.LogRecord]:
    return [r for r in records if "deny-list entries missing" in r.getMessage()]


def test_no_drift_emits_no_warning(
    session: McpSession, caplog: pytest.LogCaptureFixture
) -> None:
    """The real (undrifted) REPL_ONLY registers cleanly, no warning logged."""
    mcp = FastMCP(name="mtui-test")
    with caplog.at_level(logging.WARNING, logger="mtui.mcp.tools"):
        tools_mod.build_tools(mcp, session)

    assert _warnings(caplog.records) == []


def test_stale_deny_list_entry_warns_with_the_exact_missing_set(
    session: McpSession,
    monkeypatch: pytest.MonkeyPatch,
    caplog: pytest.LogCaptureFixture,
) -> None:
    """A REPL_ONLY entry absent from Command.registry triggers one warning.

    ``deny_present = REPL_ONLY & set(Command.registry)`` must compute the
    real intersection (an ``&`` -> ``|`` mutant would make ``deny_present``
    a superset that never equals ``REPL_ONLY``, warning on *every* call,
    including the undrifted case above); the comparison must be ``!=``
    (an ``==`` mutant flips both this test and the one above); and
    ``missing = REPL_ONLY - deny_present`` must compute the exact
    stale set (a ``None``/``+`` mutant raises before the log call, since
    frozensets support neither ``sorted(None)`` nor ``+``).
    """
    stale_name = "definitely_not_a_real_mtui_command_zzz"
    stale_repl_only = frozenset(tools_mod.REPL_ONLY | {stale_name})
    monkeypatch.setattr(tools_mod, "REPL_ONLY", stale_repl_only)
    assert stale_name not in tools_mod.Command.registry

    mcp = FastMCP(name="mtui-test")
    with caplog.at_level(logging.WARNING, logger="mtui.mcp.tools"):
        tools_mod.build_tools(mcp, session)

    warnings = _warnings(caplog.records)
    assert len(warnings) == 1
    assert warnings[0].args == ([stale_name],)
