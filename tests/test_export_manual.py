"""Tests for ``mtui.update_workflow.export.manual.ManualExport``."""

from __future__ import annotations

import logging
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.update_workflow.export.manual import ManualExport


def _config(tmp_path: Path) -> MagicMock:
    cfg = MagicMock()
    cfg.template_dir = tmp_path
    cfg.install_logs = "install_logs"
    cfg.reports_url = "https://reports"
    cfg.distro = "SLES"
    cfg.distro_ver = "15"
    cfg.distro_kernel = "5.14"
    cfg.session_user = "tester"
    return cfg


def _make(tmp_path: Path, results: list | None = None) -> ManualExport:
    cfg = _config(tmp_path)
    openqa = MagicMock()
    template: list[str] = [""]
    return ManualExport(
        cfg,
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
        results=list(results or []),
    )


# ---------------------------------------------------------------------------
# get_logs
# ---------------------------------------------------------------------------


def test_get_logs_writes_per_host_files(tmp_path: Path) -> None:
    host = MagicMock()
    host.hostname = "h1"
    host.hostlog = []
    exporter = _make(tmp_path, results=[host])
    with patch.object(exporter, "_writer") as writer:
        out = exporter.get_logs(["h1"])
    assert out == ["h1.log"]
    writer.assert_called_once()


# ---------------------------------------------------------------------------
# _host_installog_to_template
# ---------------------------------------------------------------------------


def test_host_installog_to_template_filters_zypper_and_transactional_lines(
    tmp_path: Path,
) -> None:
    host = MagicMock()
    host.hostname = "h1"
    line1 = MagicMock()
    line1.command = "zypper in bash"
    line1.stdout = "ok"
    line2 = MagicMock()
    line2.command = "ls"
    line2.stdout = "x"
    host.hostlog = [line1, line2]
    exporter = _make(tmp_path, results=[host])
    out = exporter._host_installog_to_template("h1")
    # First line: log header. Remaining: only the zypper command line.
    assert any("zypper in bash" in line for line in out)
    assert not any("ls" in line and "zypper" not in line for line in out[1:])


def test_host_installog_to_template_unknown_host_returns_empty(
    tmp_path: Path,
) -> None:
    exporter = _make(tmp_path, results=[])
    assert exporter._host_installog_to_template("missing") == []


# ---------------------------------------------------------------------------
# run pipeline
# ---------------------------------------------------------------------------


def test_run_invokes_all_pipeline_steps(tmp_path: Path) -> None:
    exporter = _make(tmp_path)
    with (
        patch.object(exporter, "install_results") as install_results,
        patch.object(exporter, "inject_openqa") as inject_openqa,
        patch.object(exporter, "inject_overview") as inject_overview,
        patch.object(exporter, "get_logs", return_value=["x.log"]) as get_logs,
        patch.object(exporter, "installlogs_lines") as installlogs_lines,
        patch.object(exporter, "add_sysinfo") as add_sysinfo,
        patch.object(exporter, "dedup_lines") as dedup_lines,
    ):
        out = exporter.run(["h1"])
    install_results.assert_called_once()
    inject_openqa.assert_called_once()
    inject_overview.assert_called_once()
    get_logs.assert_called_once_with(["h1"])
    installlogs_lines.assert_called_once_with(["x.log"])
    add_sysinfo.assert_called_once()
    dedup_lines.assert_called_once()
    assert out is exporter.template


# ---------------------------------------------------------------------------
# _fillup_hosts_to_template: install verdict (no longer script-derived)
# ---------------------------------------------------------------------------


def _pkg(name: str, before, after) -> MagicMock:
    p = MagicMock()
    p.name = name
    p.before = before
    p.after = after
    return p


def _host(packages: dict, hostname: str = "h1", system: str = "system1") -> MagicMock:
    host = MagicMock()
    host.hostname = hostname
    host.system = system
    host.packages = packages
    return host


def _host_block() -> list[str]:
    return [
        "system1 (reference host: h1)\n",
        "before:\n",
        "after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
    ]


@pytest.mark.parametrize(
    ("before", "after", "expected"),
    [
        (1, 2, "=> PASSED\n"),  # version went up -> PASSED
        (2, 2, "=> FAILED\n"),  # version unchanged -> FAILED
    ],
)
def test_fillup_flips_verdict_from_package_versions(
    tmp_path: Path, before, after, expected
) -> None:
    """The verdict derives purely from the package-version check now."""
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", before, after)})])
    exporter.template = _host_block()
    exporter._fillup_hosts_to_template()
    assert expected in exporter.template
    assert "=> PASSED/FAILED\n" not in exporter.template


def test_fillup_inserts_versions_under_indented_state_headers(
    tmp_path: Path,
) -> None:
    """Templates using the indented ``      before:`` form get the versions.

    Real (generator-produced) templates indent the state headers with six
    spaces inside each host block; the primary lookup must match them and the
    package-version lines must land directly underneath.
    """
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", 1, 2)})])
    exporter.template = [
        "system1 (reference host: h1)\n",
        "--------------\n",
        "      before:\n",
        "      after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
    ]
    exporter._fillup_hosts_to_template()
    tpl = exporter.template
    assert tpl[tpl.index("      before:\n") + 1] == "\tbash-1\n"
    assert tpl[tpl.index("      after:\n") + 1] == "\tbash-2\n"
    assert "=> PASSED\n" in tpl


def test_fillup_unindented_fallback_still_works_per_host_block(
    tmp_path: Path,
) -> None:
    """The unindented ``before:`` fallback still works, in the right block.

    One host block uses the indented headers, the following one the
    unindented (mtui-generated) form: each host's versions must land in its
    own block — the indented host must not fall through to the unindented
    lookup and write into the later host's section.
    """
    exporter = _make(
        tmp_path,
        results=[
            _host({"alpha": _pkg("alpha", 1, 2)}),
            _host({"beta": _pkg("beta", 3, 4)}, hostname="h2", system="system2"),
        ],
    )
    exporter.template = [
        "system1 (reference host: h1)\n",
        "--------------\n",
        "      before:\n",
        "      after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
        "\n",
        "system2 (reference host: h2)\n",
        "--------------\n",
        "before:\n",
        "after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
    ]
    exporter._fillup_hosts_to_template()
    tpl = exporter.template
    # h1's versions sit under h1's indented headers ...
    assert tpl[tpl.index("      before:\n") + 1] == "\talpha-1\n"
    assert tpl[tpl.index("      after:\n") + 1] == "\talpha-2\n"
    # ... and h2's under its own unindented headers (fallback path).
    assert tpl[tpl.index("before:\n") + 1] == "\tbeta-3\n"
    assert tpl[tpl.index("after:\n") + 1] == "\tbeta-4\n"


def test_fillup_unindented_block_before_indented_block_stays_isolated(
    tmp_path: Path,
) -> None:
    """Mirrored ordering: unindented host block *before* an indented one.

    This is the realistic layout that ``_fillup_hosts_to_template``'s own
    new-host-insertion loop produces: a freshly added host gets unindented
    ``before:``/``after:`` headers inserted *above* any pre-existing
    (generator-produced) indented block. Each host's versions must land in
    its own block and each host's own PASSED/FAILED placeholder must be
    flipped from that host's own package versions, rather than the first
    host's lookup overshooting into the second host's indented headers.
    """
    exporter = _make(
        tmp_path,
        results=[
            _host({"alpha": _pkg("alpha", 1, 2)}),  # increases -> PASSED
            _host(
                {"beta": _pkg("beta", 4, 3)}, hostname="h2", system="system2"
            ),  # decreases -> FAILED
        ],
    )
    exporter.template = [
        "system1 (reference host: h1)\n",
        "--------------\n",
        "before:\n",
        "after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
        "\n",
        "system2 (reference host: h2)\n",
        "--------------\n",
        "      before:\n",
        "      after:\n",
        "\n",
        "=> PASSED/FAILED\n",
        "\n",
        "comment: (none)\n",
    ]
    exporter._fillup_hosts_to_template()
    tpl = exporter.template

    h2_header = tpl.index("system2 (reference host: h2)\n")

    # h1's versions land under h1's own unindented headers, above h2 entirely ...
    h1_before = tpl.index("before:\n")
    h1_after = tpl.index("after:\n")
    assert h1_before < h2_header
    assert h1_after < h2_header
    assert tpl[h1_before + 1] == "\talpha-1\n"
    assert tpl[h1_after + 1] == "\talpha-2\n"

    # ... and h2's under its own indented headers, below h2's header, with no
    # cross-contamination between the two blocks.
    h2_before = tpl.index("      before:\n")
    h2_after = tpl.index("      after:\n")
    assert h2_before > h2_header
    assert h2_after > h2_header
    assert tpl[h2_before + 1] == "\tbeta-4\n"
    assert tpl[h2_after + 1] == "\tbeta-3\n"
    assert "\talpha-1\n" not in tpl[h2_header:]
    assert "\tbeta-4\n" not in tpl[:h2_header]

    # each host's own verdict is flipped from its own data, not the other's.
    assert "=> PASSED/FAILED\n" not in tpl
    assert "=> PASSED\n" in tpl[:h2_header]
    assert "=> FAILED\n" in tpl[h2_header:]
    assert "=> FAILED\n" not in tpl[:h2_header]
    assert "=> PASSED\n" not in tpl[h2_header:]


def test_fillup_missing_after_section_does_not_raise_keyerror(
    tmp_path: Path,
) -> None:
    """A missing ``after`` section must not crash the whole export.

    If a host's ``after:`` header cannot be located at all (e.g. a malformed
    template that never defines one), ``versions["after"]`` stays empty. The
    verdict computation must skip the version comparison for those packages
    instead of indexing into the empty dict and raising ``KeyError`` -- which
    would otherwise escape ``_fillup_hosts_to_template`` and abort the whole
    export with no template written at all.
    """
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", 1, 2)})])
    exporter.template = [
        "system1 (reference host: h1)\n",
        "before:\n",
        "\n",
        "comment: (none)\n",  # no "after:" header anywhere in this block
    ]
    # must not raise KeyError
    exporter._fillup_hosts_to_template()
    assert "\tbash-1\n" in exporter.template


def test_fillup_warns_when_version_line_cannot_be_written(
    tmp_path: Path, caplog: pytest.LogCaptureFixture
) -> None:
    """A malformed template warns per package instead of silently skipping.

    When a state header is the very last template line there is no line after
    it to inspect, so the version line cannot be written. The export must
    still complete, but with a warning naming the host and package rather
    than swallowing the error and silently omitting the line.
    """
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", 1, 2)})])
    exporter.template = [
        "system1 (reference host: h1)\n",
        "before:\n",
        "\n",
        "after:\n",  # last line: nothing after it to hold the version line
    ]
    with caplog.at_level(logging.WARNING, logger="mtui.export.manual"):
        exporter._fillup_hosts_to_template()
    warnings = [r for r in caplog.records if "malformed template" in r.getMessage()]
    assert len(warnings) == 1
    assert "bash" in warnings[0].getMessage()
    assert "h1" in warnings[0].getMessage()
    # the export still completed: the intact section was filled ...
    assert "\tbash-1\n" in exporter.template
    # ... and the unwritable one was skipped.
    assert "\tbash-2\n" not in exporter.template


def test_fillup_leaves_already_set_verdict_untouched(tmp_path: Path) -> None:
    """A re-export must not re-flip an already-decided verdict (idempotent)."""
    exporter = _make(tmp_path, results=[_host({"bash": _pkg("bash", 1, 2)})])
    template = _host_block()
    template[4] = "=> PASSED\n"  # placeholder already resolved
    exporter.template = template
    exporter._fillup_hosts_to_template()
    # the forward scan stops at the trailing ``comment:`` line without inventing
    # or re-flipping a verdict.
    assert "=> PASSED\n" in exporter.template
    assert "=> FAILED\n" not in exporter.template
