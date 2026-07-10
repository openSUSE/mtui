"""Re-export idempotency tests for the export pipeline.

Regression tests for four related bugs: every one of them made a second
`export` of the same testreport degrade the template instead of being a
no-op refresh (duplicate headers/notices, over-deletion to end of file,
dead stale-line cleanup).
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

from mtui.update_workflow.export.auto import AutoExport
from mtui.update_workflow.export.kernel import KernelExport
from mtui.update_workflow.export.manual import ManualExport

FOOTER = "## export MTUI:12.0, paramiko 3.5 on SLES-15 (kernel: 6.4) by tester\n"


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


def _manual(tmp_path: Path, template: list[str], results=None) -> ManualExport:
    return ManualExport(
        _config(tmp_path),
        MagicMock(),
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
        results=list(results or []),
    )


def _kernel(tmp_path: Path, template: list[str]) -> KernelExport:
    return KernelExport(
        _config(tmp_path),
        MagicMock(),
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
    )


# ---------------------------------------------------------------------------
# 31: auto install_results must bound its replacement at the export footer
# ---------------------------------------------------------------------------


def test_auto_install_results_bounds_at_footer(tmp_path: Path) -> None:
    """No 'Links for update logs:' section: the fallback bound is the footer.

    The footer line is '## export MTUI:<version> ...', never exactly
    '## export MTUI:', so the old list.index() fallback raised ValueError
    and the replacement ran to len(template) -- deleting the footer.
    """
    template = [
        "some intro\n",
        "##############\n",
        "Install tests:\n",
        "##############\n",
        "\n",
        "old status line\n",
        "\n",
        FOOTER,
    ]
    openqa = MagicMock()
    openqa.auto.results = []
    exporter = AutoExport(
        _config(tmp_path),
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid="SUSE:Maintenance:12358:199773",
        interactive=False,
    )

    exporter.install_results()

    assert exporter.template.count(FOOTER) == 1  # footer survived
    assert exporter.template[-1] == FOOTER
    assert "Install tests:\n" in exporter.template
    assert "old status line\n" not in exporter.template  # section body replaced


# ---------------------------------------------------------------------------
# 32: installlogs_lines must reuse an existing 'Links for update logs:' header
# ---------------------------------------------------------------------------


def test_installlogs_lines_reuses_existing_header(tmp_path: Path) -> None:
    """Re-export: no second (empty) 'Links for update logs:' section.

    The links were de-duplicated but the header was not, so manual/kernel
    re-exports stacked an empty header block per run.
    """
    rrid = "SUSE:Maintenance:12358:199773"
    old_link = f"https://reports/{rrid}/install_logs/old.log\n"
    template = [
        "body\n",
        "\n",
        "Links for update logs:\n",
        "\n",
        old_link,
        "\n",
        FOOTER,
    ]
    exporter = _manual(tmp_path, template)

    exporter.installlogs_lines(["old.log", "new.log"])

    tpl = list(exporter.template)
    assert tpl.count("Links for update logs:\n") == 1
    assert tpl.count(old_link) == 1  # still de-duplicated
    new_link = f"https://reports/{rrid}/install_logs/new.log\n"
    assert tpl.count(new_link) == 1
    # both links live under the one header, before the footer
    header = tpl.index("Links for update logs:\n")
    assert header < tpl.index(new_link) < tpl.index(FOOTER)


def test_installlogs_lines_twice_is_idempotent(tmp_path: Path) -> None:
    """Two identical calls (two exports) leave exactly one section."""
    template = ["body\n", FOOTER]
    exporter = _manual(tmp_path, template)

    exporter.installlogs_lines(["a.log"])
    exporter.installlogs_lines(["a.log"])

    tpl = list(exporter.template)
    assert tpl.count("Links for update logs:\n") == 1
    rrid = "SUSE:Maintenance:12358:199773"
    assert tpl.count(f"https://reports/{rrid}/install_logs/a.log\n") == 1


# ---------------------------------------------------------------------------
# 33: base install_results notice must not multiply on kernel re-export
# ---------------------------------------------------------------------------


def test_kernel_install_notice_not_duplicated_on_reexport(tmp_path: Path) -> None:
    notice = "All installation tests done in openQA please see installlogs section\n"
    template = [
        "Test results by product-arch:\n",
        "(x)\n",
        "(y)\n",
        "\n",
        "tail\n",
    ]
    exporter = _kernel(tmp_path, template)

    exporter.install_results()
    exporter.install_results()  # second export

    assert list(exporter.template).count(notice) == 1


# ---------------------------------------------------------------------------
# 34: manual install_results stale-result cleanup must actually work
# ---------------------------------------------------------------------------


def _session_host(hostname: str, system: str) -> MagicMock:
    host = MagicMock()
    host.hostname = hostname
    host.system = system
    host.packages = {}
    return host


def test_manual_install_results_strips_stale_lines_for_session_hosts(
    tmp_path: Path,
) -> None:
    """Stale per-command result lines are refreshed for session hosts only.

    The old tracking regex required two spaces after 'reference host:'
    (the template emits one) and read group(0), so c_host never matched a
    bare hostname and no stale line was ever removed.
    """
    template = [
        "system1 (reference host: h1)\n",
        "old-cmd : FAILED\n",
        "good line\n",
        "othersys (reference host: elsewhere)\n",
        "their-cmd : SUCCEEDED\n",
    ]
    exporter = _manual(tmp_path, template, results=[_session_host("h1", "system1")])

    exporter.install_results()

    tpl = list(exporter.template)
    # both host section headers survive
    assert "system1 (reference host: h1)\n" in tpl
    assert "othersys (reference host: elsewhere)\n" in tpl
    # the session host's stale result line is gone, its other content kept
    assert "old-cmd : FAILED\n" not in tpl
    assert "good line\n" in tpl
    # a host NOT in this session keeps its result lines untouched
    assert "their-cmd : SUCCEEDED\n" in tpl


def test_manual_cleanup_stops_at_host_section_end(tmp_path: Path) -> None:
    """The deletion window must not bleed past the host block.

    c_host used to stay armed after the last host section, so
    tester-authored lines like 'reproducer : FAILED before update' in the
    regression-tests notes were silently deleted on every export
    (adversarial-review catch on the revived cleanup).
    """
    template = [
        "system1 (reference host: h1)\n",
        "old-cmd : FAILED\n",
        "comment: (none)\n",
        "\n",
        "regression tests:\n",
        "=================\n",
        "bsc#1234 reproducer : FAILED before update\n",
        "bsc#1234 reproducer : SUCCEEDED after update\n",
        "\n",
    ]
    exporter = _manual(tmp_path, template, results=[_session_host("h1", "system1")])

    exporter.install_results()

    tpl = list(exporter.template)
    assert "old-cmd : FAILED\n" not in tpl  # in-section stale line removed
    # tester content after the host block survives
    assert "bsc#1234 reproducer : FAILED before update\n" in tpl
    assert "bsc#1234 reproducer : SUCCEEDED after update\n" in tpl
    assert "comment: (none)\n" in tpl


def test_installlogs_trailing_blank_does_not_accumulate(tmp_path: Path) -> None:
    """A re-export that adds one new link must not add another blank line.

    The section already ends with a blank; the trailing-blank insert is
    guarded, or every kernel re-export with a new arch log would grow the
    gap before the footer (adversarial-review catch: the guard was
    untested).
    """
    rrid = "SUSE:Maintenance:12358:199773"
    old_link = f"https://reports/{rrid}/install_logs/old.log\n"
    template = [
        "Links for update logs:\n",
        "\n",
        old_link,
        "\n",
        FOOTER,
    ]
    exporter = _manual(tmp_path, template)

    exporter.installlogs_lines(["new.log"])

    tpl = list(exporter.template)
    new_link = f"https://reports/{rrid}/install_logs/new.log\n"
    assert tpl.count(new_link) == 1
    # exactly one blank between the links and the footer
    footer_at = tpl.index(FOOTER)
    assert tpl[footer_at - 1] == "\n"
    assert tpl[footer_at - 2] != "\n"


def test_installlogs_converges_damaged_stacked_headers(tmp_path: Path) -> None:
    """Templates damaged by the old bug converge back to one section.

    Pre-fix exports stacked one empty header per run; nothing ever
    removed them (dedup_lines only collapses adjacent identical non-blank
    lines), so the junk was re-uploaded with every future export.
    """
    rrid = "SUSE:Maintenance:12358:199773"
    old_link = f"https://reports/{rrid}/install_logs/old.log\n"
    template = [
        "body\n",
        "\n",
        "Links for update logs:\n",
        "\n",
        old_link,
        "\n",
        "\n",
        "Links for update logs:\n",
        "\n",
        "\n",
        "Links for update logs:\n",
        "\n",
        FOOTER,
    ]
    exporter = _manual(tmp_path, template)

    exporter.installlogs_lines(["old.log"])

    tpl = list(exporter.template)
    assert tpl.count("Links for update logs:\n") == 1
    assert tpl.count(old_link) == 1
    assert tpl[-1] == FOOTER


def test_installlogs_hand_trimmed_section_keeps_links_before_footer(
    tmp_path: Path,
) -> None:
    """Header directly followed by the footer: links must not land after it.

    A hand-edited template whose empty links section lost its blank line
    used to get the new link inserted AFTER the '## export MTUI:' footer
    (adversarial-review catch on the reuse walk).
    """
    template = [
        "body\n",
        "Links for update logs:\n",
        FOOTER,
    ]
    exporter = _manual(tmp_path, template)

    exporter.installlogs_lines(["a.log"])

    tpl = list(exporter.template)
    rrid = "SUSE:Maintenance:12358:199773"
    link = f"https://reports/{rrid}/install_logs/a.log\n"
    assert tpl.index("Links for update logs:\n") < tpl.index(link) < tpl.index(FOOTER)
    assert tpl[-1] == FOOTER


def test_kernel_install_notice_converges_from_damaged_template(
    tmp_path: Path,
) -> None:
    """Notices stacked by pre-fix exports are reduced back to one."""
    notice = "All installation tests done in openQA please see installlogs section\n"
    template = [
        "Test results by product-arch:\n",
        "(x)\n",
        "(y)\n",
        notice,
        "\n",
        notice,
        "\n",
        notice,
        "\n",
        "tail\n",
    ]
    exporter = _kernel(tmp_path, template)

    exporter.install_results()

    tpl = list(exporter.template)
    assert tpl.count(notice) == 1
    assert "tail\n" in tpl
