"""Mutation-killing pins for ``mtui.update_workflow.export.base``.

A full mutmut run left survivors in code the export suite executes but
never asserts on. These tests pin the exact template layout produced by
``BaseExport.inject_openqa``, ``installlogs_lines``, ``_writer``,
``add_sysinfo`` and ``dedup_lines`` (full-list equality instead of the
membership checks the older tests use), so structural mutants -- index
arithmetic, marker text, blank-line framing, prompt handling -- no
longer survive.

All fixtures are local; no network, no chdir, everything under
``tmp_path``.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

from mtui.support.systemcheck import system_info
from mtui.update_workflow.export.base import BaseExport

RRID = "SUSE:Maintenance:12358:199773"
FOOTER = "## export MTUI:12.0, paramiko 3.5 on SLES-15 (kernel: 6.4) by tester\n"
END_MARKER = "End of openQA Incidents results\n"
SCCR = "source code change review:\n"
LINKS = "Links for update logs:\n"


class _Probe(BaseExport):
    """Concrete BaseExport so the shared helpers can be driven directly."""

    def get_logs(self, *args, **kwds) -> list[Path]:
        return []

    def run(self, *args, **kwds):
        return self.template


def _config(tmp_path: Path | None = None) -> MagicMock:
    cfg = MagicMock()
    if tmp_path is not None:
        cfg.template_dir = tmp_path
    cfg.install_logs = "install_logs"
    cfg.reports_url = "https://reports"
    cfg.distro = "SLES"
    cfg.distro_ver = "15"
    cfg.distro_kernel = "5.14"
    cfg.session_user = "tester"
    return cfg


def _probe(
    template: list[str],
    *,
    tmp_path: Path | None = None,
    auto_pp: list[str] | None = None,
    force: bool = False,
    interactive: bool = False,
) -> _Probe:
    openqa = MagicMock()
    if auto_pp is not None:
        openqa.auto.pp = auto_pp
    return _Probe(
        _config(tmp_path),
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=force,
        rrid=RRID,
        interactive=interactive,
    )


def _link(name: str) -> str:
    return f"https://reports/{RRID}/install_logs/{name}\n"


# ---------------------------------------------------------------------------
# inject_openqa: golden-list pins (position, marker text, legacy titles,
# missing-end-marker fallback)
# ---------------------------------------------------------------------------

_PP = ["Results from openQA jobs:\n", "new result\n"]

# What one injection into a section-less template body looks like: the pp
# lines land immediately before the blank line preceding the
# 'source code change review:' header, then the end-marker trio follows.
_INJECTED = [
    "intro\n",
    "Results from openQA jobs:\n",
    "new result\n",
    "\n",
    END_MARKER,
    "\n",
    "\n",
    SCCR,
]


def test_inject_openqa_replaces_current_section_exact_layout() -> None:
    """Old section (title..end marker) removed, new block at exact position."""
    exporter = _probe(
        [
            "intro\n",
            "Results from openQA jobs:\n",
            "old result\n",
            END_MARKER,
            "\n",
            SCCR,
        ],
        auto_pp=_PP,
    )

    exporter.inject_openqa()

    assert list(exporter.template) == _INJECTED


def test_inject_openqa_second_run_reuses_own_end_marker() -> None:
    """The next export's removal finds the marker the previous one wrote.

    The steady state is not a perfect no-op: each cycle leaves one extra
    blank line behind (removal keeps the post-marker blank while the
    insertion adds a fresh framing pair). Pinning the exact second-run
    list keeps the marker text and the removal window honest.
    """
    exporter = _probe(
        [
            "intro\n",
            "Results from openQA jobs:\n",
            "old result\n",
            END_MARKER,
            "\n",
            SCCR,
        ],
        auto_pp=_PP,
    )

    exporter.inject_openqa()
    exporter.inject_openqa()

    assert list(exporter.template) == [
        "intro\n",
        "\n",
        "Results from openQA jobs:\n",
        "new result\n",
        "\n",
        END_MARKER,
        "\n",
        "\n",
        SCCR,
    ]


def test_inject_openqa_removes_legacy_incidents_openqa_title() -> None:
    """'Results from incidents openQA jobs:' sections are still replaced."""
    exporter = _probe(
        [
            "intro\n",
            "Results from incidents openQA jobs:\n",
            "old result\n",
            END_MARKER,
            "\n",
            SCCR,
        ],
        auto_pp=_PP,
    )

    exporter.inject_openqa()

    assert list(exporter.template) == _INJECTED


def test_inject_openqa_removes_legacy_openqa_incidents_title() -> None:
    """'Results from openQA incidents jobs:' sections are still replaced."""
    exporter = _probe(
        [
            "intro\n",
            "Results from openQA incidents jobs:\n",
            "old result\n",
            END_MARKER,
            "\n",
            SCCR,
        ],
        auto_pp=_PP,
    )

    exporter.inject_openqa()

    assert list(exporter.template) == _INJECTED


def test_inject_openqa_missing_end_marker_bounds_at_review_header() -> None:
    """Without the end marker the removal stops just above 'source code...'."""
    exporter = _probe(
        [
            "intro\n",
            "Results from openQA jobs:\n",
            "old result\n",
            "\n",
            SCCR,
        ],
        auto_pp=_PP,
    )

    exporter.inject_openqa()

    assert list(exporter.template) == _INJECTED


# ---------------------------------------------------------------------------
# installlogs_lines: exact-layout pins for the header-reuse walk, the
# footer fallback, the hand-trimmed guard and the HAS_UNTRACKED offset
# ---------------------------------------------------------------------------


def test_installlogs_appends_new_link_into_existing_section_exact() -> None:
    exporter = _probe(["body\n", "\n", LINKS, "\n", _link("old.log"), "\n", FOOTER])

    exporter.installlogs_lines(["old.log", "new.log"])

    assert list(exporter.template) == [
        "body\n",
        "\n",
        LINKS,
        "\n",
        _link("old.log"),
        _link("new.log"),
        "\n",
        FOOTER,
    ]


def test_installlogs_twice_is_idempotent_exact() -> None:
    exporter = _probe(["body\n", FOOTER])

    exporter.installlogs_lines(["a.log"])
    first = list(exporter.template)
    exporter.installlogs_lines(["a.log"])

    assert first == [
        "body\n",
        "\n",
        LINKS,
        "\n",
        _link("a.log"),
        "\n",
        FOOTER,
    ]
    assert list(exporter.template) == first


def test_installlogs_converges_damaged_stacked_headers_exact() -> None:
    exporter = _probe(
        [
            "body\n",
            "\n",
            LINKS,
            "\n",
            _link("old.log"),
            "\n",
            "\n",
            LINKS,
            "\n",
            "\n",
            LINKS,
            "\n",
            FOOTER,
        ]
    )

    exporter.installlogs_lines(["old.log"])

    assert list(exporter.template) == [
        "body\n",
        "\n",
        LINKS,
        "\n",
        _link("old.log"),
        FOOTER,
    ]


def test_installlogs_new_section_lands_before_trailing_footer() -> None:
    """The footer check reads the LAST line, not line 1 of the template."""
    exporter = _probe(["body1\n", "body2\n", "body3\n", FOOTER])

    exporter.installlogs_lines(["a.log"])

    assert list(exporter.template) == [
        "body1\n",
        "body2\n",
        "body3\n",
        "\n",
        LINKS,
        "\n",
        _link("a.log"),
        "\n",
        FOOTER,
    ]


def test_installlogs_header_directly_followed_by_link_untouched() -> None:
    """No spurious blank is inserted when links follow the header directly."""
    template = ["body\n", LINKS, _link("old.log"), "\n", FOOTER]
    exporter = _probe(list(template))

    exporter.installlogs_lines(["old.log"])

    assert list(exporter.template) == template


def test_installlogs_hand_trimmed_header_gets_canonical_blank_exact() -> None:
    """Header directly followed by the footer: restored blank, link inside."""
    exporter = _probe(["body\n", LINKS, FOOTER])

    exporter.installlogs_lines(["a.log"])

    assert list(exporter.template) == [
        "body\n",
        LINKS,
        "\n",
        _link("a.log"),
        "\n",
        FOOTER,
    ]


def test_installlogs_dedup_window_starts_right_after_untracked_marker() -> None:
    """A link on the line directly after HAS_UNTRACKED suppresses re-adding."""
    template = [
        "HAS_UNTRACKED_CHANGES: NO\n",
        _link("x.log"),
        "\n",
        LINKS,
        "\n",
        FOOTER,
    ]
    exporter = _probe(list(template))

    exporter.installlogs_lines(["x.log"])

    assert list(exporter.template) == template


# ---------------------------------------------------------------------------
# _writer: the exists-and-not-force paths (same content, prompt accepted,
# prompt declined, force, OSError)
# ---------------------------------------------------------------------------


def test_writer_same_content_returns_without_prompting(tmp_path: Path) -> None:
    fn = tmp_path / "h1.log"
    fn.write_text("a\nb")
    exporter = _probe([""], tmp_path=tmp_path)

    with patch("mtui.update_workflow.export.base.prompt_user") as prompt:
        exporter._writer(fn, ["a", "b"])

    prompt.assert_not_called()
    assert fn.read_text() == "a\nb"
    assert list(tmp_path.iterdir()) == [fn]  # no timestamp sidecar


def test_writer_force_overwrites_without_prompting(tmp_path: Path) -> None:
    fn = tmp_path / "h1.log"
    fn.write_text("old")
    exporter = _probe([""], tmp_path=tmp_path, force=True)

    with patch("mtui.update_workflow.export.base.prompt_user") as prompt:
        exporter._writer(fn, ["new"])

    prompt.assert_not_called()
    assert fn.read_text() == "new"


def test_writer_prompt_accepted_overwrites(tmp_path: Path) -> None:
    fn = tmp_path / "h1.log"
    fn.write_text("old")
    exporter = _probe([""], tmp_path=tmp_path, interactive=True)

    with patch(
        "mtui.update_workflow.export.base.prompt_user", return_value=True
    ) as prompt:
        exporter._writer(fn, ["new"])

    assert fn.read_text() == "new"
    # The accepted answers and the interactive flag are part of the
    # behavioral contract (which input overwrites a tester's file).
    args = prompt.call_args.args
    assert args[1] == ["y", "Y", "yes", "Yes", "YES"]
    assert args[2] is True


def test_writer_prompt_declined_writes_timestamp_sidecar(tmp_path: Path) -> None:
    fn = tmp_path / "h1.log"
    fn.write_text("old")
    exporter = _probe([""], tmp_path=tmp_path)

    with (
        patch("mtui.update_workflow.export.base.prompt_user", return_value=False),
        patch(
            "mtui.update_workflow.export.base.timestamp",
            return_value="1111111111",
        ),
    ):
        exporter._writer(fn, ["new"])

    assert fn.read_text() == "old"  # original untouched
    sidecar = tmp_path / "h1.1111111111"
    assert sidecar.read_text() == "new"


def test_writer_oserror_is_logged_not_raised(tmp_path: Path, caplog) -> None:
    fn = tmp_path / "missing-dir" / "x.log"
    exporter = _probe([""], tmp_path=tmp_path)

    with caplog.at_level("ERROR", logger="mtui.export.base"):
        exporter._writer(fn, ["data"])  # must not raise

    assert any("Failed to write" in r.message for r in caplog.records)
    assert not fn.exists()


# ---------------------------------------------------------------------------
# add_sysinfo: the exact footer and its four config-derived arguments
# ---------------------------------------------------------------------------


def test_add_sysinfo_appends_exact_footer() -> None:
    exporter = _probe(["a\n", "b\n"])
    expected = system_info("SLES", "15", "5.14", "tester")

    exporter.add_sysinfo()

    assert list(exporter.template) == ["a\n", "b\n", expected]


# ---------------------------------------------------------------------------
# dedup_lines: adjacent duplicate text lines collapse, blank runs survive
# ---------------------------------------------------------------------------


def test_dedup_lines_collapses_text_duplicates_keeps_blank_runs() -> None:
    exporter = _probe(["a\n", "a\n", "\n", "\n", "b\n", "b\n", "b\n", "\n"])

    exporter.dedup_lines()

    assert list(exporter.template) == ["a\n", "\n", "\n", "b\n", "\n"]
