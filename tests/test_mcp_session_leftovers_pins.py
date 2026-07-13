"""Mutation-killing pins for small leftovers in :mod:`mtui.mcp.session`.

Two narrow gaps a full mutmut run left unasserted:

* ``McpSession.start_jobs``: when an explicit ``-T <rrid>`` narrows the
  fan-out to a single template, ``_resolve_job_rrids`` returns exactly one
  rrid and ``len(rrids) <= 1`` takes the single-job path (the stable
  ``<command>-<n>`` id shape, with no RRID token embedded). The existing
  ``test_start_jobs_explicit_template_yields_single_job`` (in
  ``tests/test_mcp_jobs.py``) asserts only the job count and output, not
  the id shape, so a ``<=`` -> ``<`` mutant (which pushes this case onto
  the fan-out path instead) survives. This module re-drives that same
  scenario and additionally pins the id shape.
* ``job_status``'s unknown-id refusal raises the same uniform
  :class:`McpCommandError` envelope (``stdout=""``, ``exit_code=1``)
  every other tool refusal uses, but the existing
  ``test_job_status_unknown_id_raises`` only regex-matches the stderr
  message, so mutants on the envelope's ``stdout``/``exit_code``
  constants survive. ``job_result`` and ``job_cancel`` raise the
  identical envelope for the same unknown-id case, so one parametrized
  test pins all three.
"""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from typing import ClassVar
from unittest.mock import MagicMock

import pytest

from mtui.commands import Command
from mtui.mcp.session import McpCommandError, McpSession


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.target_tempdir = tmp_path / "target"
    cfg.chdir_to_template_dir = False
    cfg.connection_timeout = 30
    cfg.session_user = "testuser"
    return cfg


def _make_session(tmp_path: Path) -> McpSession:
    return McpSession(
        _config(tmp_path), logging.getLogger("test.mcp.session.leftovers")
    )


def _load_two_reports(sess: McpSession) -> None:
    """Add two MagicMock reports so a fan-out command resolves to both."""
    for rrid in ("SUSE:Maintenance:1:1", "SUSE:Maintenance:2:1"):
        report = MagicMock()
        report.id = rrid
        report.targets = {}
        sess.templates.add(report)
    sess.templates.set_active("SUSE:Maintenance:1:1")


def _fanout_probe():
    """Build a throwaway fast fan-out command; caller must unregister it."""

    class _FanoutJobProbeLeftovers(Command):
        command = "fanout_job_probe_leftovers_tmp"
        scope: ClassVar[str] = "fanout"

        @classmethod
        def _add_arguments(cls, parser) -> None:
            cls._add_template_arg(parser)

        def __call__(self) -> None:
            self.println(str(self.metadata.id))

    return _FanoutJobProbeLeftovers


# --------------------------------------------------------------------------- #
# start_jobs: explicit -T narrowing keeps the single-job id shape             #
# --------------------------------------------------------------------------- #


def test_start_jobs_explicit_template_keeps_the_single_job_id_shape(
    tmp_path: Path,
) -> None:
    """A client-supplied ``-T`` narrows to one template: legacy id, no RRID.

    ``len(rrids) <= 1`` must route here even though ``rrids`` is
    non-empty; a ``<=`` -> ``<`` mutant instead sends this down the
    fan-out path, which embeds the (sanitised) RRID in the job id.
    """
    sess = _make_session(tmp_path)
    _load_two_reports(sess)
    cls = _fanout_probe()

    async def driver() -> list[str]:
        ids = await sess.start_jobs(cls, ["-T", "SUSE:Maintenance:2:1"])
        for jid in ids:
            await sess._jobs[jid]["task"]
        return ids

    try:
        ids = asyncio.run(driver())
    finally:
        Command.registry.pop(cls.command, None)

    assert len(ids) == 1
    assert ids[0].startswith(f"{cls.command}-")
    assert "SUSE" not in ids[0]
    assert sess.job_result(ids[0]).strip() == "SUSE:Maintenance:2:1"


# --------------------------------------------------------------------------- #
# Unknown job id: the uniform McpCommandError envelope                       #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "invoke",
    [
        lambda s: s.job_status("nope-1"),
        lambda s: s.job_result("nope-1"),
        lambda s: asyncio.run(s.job_cancel("nope-1")),
    ],
    ids=["job_status", "job_result", "job_cancel"],
)
def test_unknown_job_id_raises_the_uniform_envelope(invoke, tmp_path: Path) -> None:
    """Every unknown-id lookup raises stdout='', exit_code=1 verbatim."""
    sess = _make_session(tmp_path)

    with pytest.raises(McpCommandError) as ei:
        invoke(sess)

    assert ei.value.stdout == ""
    assert ei.value.stderr == "no such job: nope-1"
    assert ei.value.exit_code == 1
