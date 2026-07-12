"""Mutation-killing pins for the manual/auto/kernel export workflows.

Survivors from a full mutmut run showed the host-section creation in
``ManualExport._fillup_hosts_to_template``, the section replacement in
``AutoExport.install_results``, the regression-section rewrite in
``KernelExport.kernel_results`` and the ``get_logs`` call contracts were
executed by the suite but never asserted on. These tests pin the exact
template layouts and call arguments those paths produce today.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import ANY, MagicMock, patch

from mtui.support.http import HTTP_TIMEOUT
from mtui.types import URLs
from mtui.update_workflow.export.auto import AutoExport
from mtui.update_workflow.export.kernel import KernelExport
from mtui.update_workflow.export.manual import ManualExport

RRID = "SUSE:Maintenance:12358:199773"
FOOTER = "## export MTUI:12.0, paramiko 3.5 on SLES-15 (kernel: 6.4) by tester\n"


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
    cfg.ssl_verify = True
    return cfg


# ---------------------------------------------------------------------------
# ManualExport._fillup_hosts_to_template
# ---------------------------------------------------------------------------


def _pkg(name: str, before, after) -> MagicMock:
    p = MagicMock()
    p.name = name
    p.before = before
    p.after = after
    return p


def _host(hostname: str, system: str, packages: dict) -> MagicMock:
    h = MagicMock()
    h.hostname = hostname
    h.system = system
    h.packages = packages
    return h


def _manual(template: list[str], results: list) -> ManualExport:
    return ManualExport(
        _config(),
        MagicMock(),
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
        results=results,
    )


def test_fillup_creates_full_host_section_under_product_arch_anchor() -> None:
    """Missing host section: the whole block is created two lines below the
    'Test results by product-arch:' anchor, package versions included."""
    exporter = _manual(
        [
            "Test results by product-arch:\n",
            "#############################\n",
            "\n",
            "tail\n",
        ],
        [_host("h1", "system1", {"bash": _pkg("bash", 2, 3)})],
    )

    exporter._fillup_hosts_to_template()

    assert list(exporter.template) == [
        "Test results by product-arch:\n",
        "#############################\n",
        "\n",
        "system1 (reference host: h1)\n",
        "--------------\n",
        "before:\n",
        "\tbash-2\n",
        "after:\n",
        "\tbash-3\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
        "\n",
        "\n",
        "tail\n",
    ]


def test_fillup_creates_host_section_under_deprecated_platform_anchor() -> None:
    """Older templates use 'Test results by test platform:' as the anchor."""
    exporter = _manual(
        [
            "Test results by test platform:\n",
            "====\n",
            "\n",
            "tail\n",
        ],
        [_host("h1", "system1", {"bash": _pkg("bash", 2, 3)})],
    )

    exporter._fillup_hosts_to_template()

    assert list(exporter.template) == [
        "Test results by test platform:\n",
        "====\n",
        "\n",
        "system1 (reference host: h1)\n",
        "--------------\n",
        "before:\n",
        "\tbash-2\n",
        "after:\n",
        "\tbash-3\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
        "\n",
        "\n",
        "tail\n",
    ]


def test_fillup_substitutes_hostname_into_placeholder_host_line() -> None:
    """A 'reference host: ?' section is claimed for the session host."""
    exporter = _manual(
        [
            "system1 (reference host: ?)\n",
            "--------------\n",
            "before:\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ],
        [_host("h1", "system1", {"bash": _pkg("bash", 2, 3)})],
    )

    exporter._fillup_hosts_to_template()

    assert list(exporter.template) == [
        "system1 (reference host: h1)\n",
        "--------------\n",
        "before:\n",
        "\tbash-2\n",
        "after:\n",
        "\tbash-3\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
    ]


def test_fillup_overwrites_stale_version_and_marks_missing_package() -> None:
    """An already-exported version line is overwritten in place; a package
    with no 'before' version gets the 'is not installed' line."""
    exporter = _manual(
        [
            "system1 (reference host: h1)\n",
            "--------------\n",
            "before:\n",
            "\tbash-1\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ],
        [
            _host(
                "h1",
                "system1",
                {"bash": _pkg("bash", 2, 3), "zsh": _pkg("zsh", None, 5)},
            )
        ],
    )

    exporter._fillup_hosts_to_template()

    assert list(exporter.template) == [
        "system1 (reference host: h1)\n",
        "--------------\n",
        "before:\n",
        "\tbash-2\n",
        "\tpackage zsh is not installed\n",
        "after:\n",
        "\tbash-3\n",
        "\tzsh-5\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
    ]


def test_fillup_fills_both_hosts_and_flips_both_verdicts() -> None:
    """Two session hosts: each section gets its own versions and verdict."""
    exporter = _manual(
        [
            "sysA (reference host: hA)\n",
            "--------------\n",
            "before:\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
            "\n",
            "sysB (reference host: hB)\n",
            "--------------\n",
            "before:\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ],
        [
            _host("hA", "sysA", {"bash": _pkg("bash", 1, 2)}),
            _host("hB", "sysB", {"bash": _pkg("bash", 2, 2)}),
        ],
    )

    exporter._fillup_hosts_to_template()

    assert list(exporter.template) == [
        "sysA (reference host: hA)\n",
        "--------------\n",
        "before:\n",
        "\tbash-1\n",
        "after:\n",
        "\tbash-2\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
        "\n",
        "sysB (reference host: hB)\n",
        "--------------\n",
        "before:\n",
        "\tbash-2\n",
        "after:\n",
        "\tbash-2\n",
        "\n",
        "=> FAILED\n",
        "\n",
        "comment: (none)\n",
    ]


def test_fillup_missing_anchor_still_fills_later_hosts(caplog) -> None:
    """No anchor for the first host aborts section creation (break, not
    return): the second host's pre-existing section is still filled."""
    exporter = _manual(
        [
            "sysB (reference host: hB)\n",
            "--------------\n",
            "before:\n",
            "after:\n",
            "\n",
            "=> PASSED/FAILED\n",
            "\n",
            "comment: (none)\n",
        ],
        [
            _host("hA", "sysA", {"bash": _pkg("bash", 1, 2)}),
            _host("hB", "sysB", {"bash": _pkg("bash", 1, 2)}),
        ],
    )

    with caplog.at_level("ERROR", logger="mtui.export.manual"):
        exporter._fillup_hosts_to_template()

    assert any("update results section not found" in r.message for r in caplog.records)
    assert list(exporter.template) == [
        "sysB (reference host: hB)\n",
        "--------------\n",
        "before:\n",
        "\tbash-1\n",
        "after:\n",
        "\tbash-2\n",
        "\n",
        "=> PASSED\n",
        "\n",
        "comment: (none)\n",
    ]


# ---------------------------------------------------------------------------
# ManualExport.get_logs: the written file path equals the linked name
# ---------------------------------------------------------------------------


def test_manual_get_logs_writes_exact_path_and_filtered_content(
    tmp_path: Path,
) -> None:
    host = MagicMock()
    host.hostname = "h1"
    zypper = MagicMock()
    zypper.command = "zypper in bash"
    zypper.stdout = "ok"
    tu = MagicMock()
    tu.command = "transactional-update pkg install foo"
    tu.stdout = "applied"
    noise = MagicMock()
    noise.command = "ls"
    noise.stdout = "junk"
    host.hostlog = [zypper, noise, tu]

    exporter = ManualExport(
        _config(tmp_path),
        MagicMock(),
        [""],  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
        results=[host],
    )

    with patch.object(exporter, "_writer") as writer:
        out = exporter.get_logs(["h1"])

    assert out == ["h1.log"]
    writer.assert_called_once_with(
        tmp_path / RRID / "install_logs" / "h1.log",
        [
            "log from h1:\n",
            "# zypper in bash\nok\n",
            "# transactional-update pkg install foo\napplied\n",
        ],
    )


# ---------------------------------------------------------------------------
# AutoExport.install_results: exact section replacement and creation
# ---------------------------------------------------------------------------

_URL = URLs(
    "sle",
    "x86_64",
    "15-SP7",
    "https://openqa.example.com/tests/1001/file/install-logs.tar",
    "passed",
)
_STATUS = "Installation tests done in openQA with following results: PASSED\n"
_JOB = "sle_15-SP7_x86_64 => PASSED: https://openqa.example.com/tests/1001\n"


def _auto(template: list[str], results: list, force: bool = False) -> AutoExport:
    openqa = MagicMock()
    openqa.auto.results = results
    return AutoExport(
        _config(),
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=force,
        rrid=RRID,
        interactive=False,
    )


def test_auto_install_results_replaces_section_exact_layout() -> None:
    """The stale section body is replaced in place: exactly one banner, one
    'Install tests:' header, and the fresh status/job lines."""
    exporter = _auto(
        [
            "intro\n",
            "##############\n",
            "Install tests:\n",
            "##############\n",
            "\n",
            "old status\n",
            "\n",
            "old job line\n",
            "\n",
            "Links for update logs:\n",
            "\n",
            FOOTER,
        ],
        [_URL],
    )

    exporter.install_results()

    assert exporter.template.count("Install tests:\n") == 1
    assert list(exporter.template) == [
        "intro\n",
        "##############\n",
        "Install tests:\n",
        "##############\n",
        "\n",
        _STATUS,
        "\n",
        _JOB,
        "\n",
        "\n",
        "Links for update logs:\n",
        "\n",
        FOOTER,
    ]


def test_auto_install_results_creates_section_before_footer() -> None:
    exporter = _auto(["intro\n", FOOTER], [_URL])

    exporter.install_results()

    assert list(exporter.template) == [
        "intro\n",
        "##############\n",
        "Install tests:\n",
        "##############\n",
        "\n",
        _STATUS,
        "\n",
        _JOB,
        "\n",
        FOOTER,
    ]


def test_auto_install_results_creates_section_at_eof_without_footer() -> None:
    exporter = _auto(["intro\n"], [_URL])

    exporter.install_results()

    assert list(exporter.template) == [
        "intro\n",
        "##############\n",
        "Install tests:\n",
        "##############\n",
        "\n",
        _STATUS,
        "\n",
        _JOB,
        "\n",
    ]


# ---------------------------------------------------------------------------
# AutoExport.run: the install_logs_current guard around get_logs
# ---------------------------------------------------------------------------

# NOTE: the guard tests list membership, not substring containment, so only
# a template LINE that equals the sentinel exactly (no status suffix, no
# newline) marks the logs as current. Real status lines carry a suffix and
# never match -- pinned below as current behavior.
_SENTINEL = "Installation tests done in openQA with following results:"


def _run_auto(template: list[str], force: bool) -> tuple[MagicMock, MagicMock]:
    exporter = _auto(template, [], force=force)
    with (
        patch.object(exporter, "install_results"),
        patch.object(exporter, "inject_openqa"),
        patch.object(exporter, "inject_overview"),
        patch.object(exporter, "get_logs", return_value=[]) as get_logs,
        patch.object(exporter, "installlogs_lines") as installlogs,
        patch.object(exporter, "add_sysinfo"),
        patch.object(exporter, "dedup_lines"),
    ):
        exporter.run()
    return get_logs, installlogs


def test_auto_run_skips_log_download_when_results_line_current() -> None:
    get_logs, installlogs = _run_auto(["intro\n", _SENTINEL, "tail\n"], force=False)

    get_logs.assert_not_called()
    installlogs.assert_not_called()


def test_auto_run_force_redownloads_logs() -> None:
    get_logs, installlogs = _run_auto(["intro\n", _SENTINEL, "tail\n"], force=True)

    get_logs.assert_called_once()
    installlogs.assert_called_once_with([])


def test_auto_run_downloads_logs_when_results_line_absent() -> None:
    get_logs, installlogs = _run_auto(["intro\n", "tail\n"], force=False)

    get_logs.assert_called_once()
    installlogs.assert_called_once_with([])


def test_auto_run_status_suffixed_line_does_not_count_as_current() -> None:
    """Membership is per-line equality: a real '... results: PASSED' line
    does not satisfy the guard, so logs are (re)downloaded."""
    get_logs, _ = _run_auto(["intro\n", _STATUS, "tail\n"], force=False)

    get_logs.assert_called_once()


# ---------------------------------------------------------------------------
# AutoExport._openqa_installog_to_template: unset ssl_verify stays secure,
# and the request targets the result's URL
# ---------------------------------------------------------------------------


def test_installog_download_unset_ssl_verify_defaults_to_verifying() -> None:
    cfg = _config()
    cfg.ssl_verify = None
    exporter = AutoExport(
        cfg,
        MagicMock(),
        [],  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
    )
    response = MagicMock()
    response.text = "line\n"
    response.raise_for_status.return_value = None
    session = MagicMock()
    session.get.return_value = response

    with patch(
        "mtui.update_workflow.export.auto.build_session", return_value=session
    ) as build_session:
        out = exporter._openqa_installog_to_template(_URL)

    assert out == ["line\n"]
    build_session.assert_called_once_with(True)
    session.get.assert_called_once_with(_URL.url, timeout=HTTP_TIMEOUT)


# ---------------------------------------------------------------------------
# KernelExport.kernel_results: placeholder replacement preserves tester
# notes; re-export bulk-replaces the section; legacy pes.suse.de anchor
# ---------------------------------------------------------------------------


def _kernel(template: list[str], pp_groups: list[list[str]]) -> KernelExport:
    openqa = MagicMock()
    groups = []
    for pp in pp_groups:
        g = MagicMock()
        g.pp = pp
        groups.append(g)
    openqa.kernel = groups
    return KernelExport(
        _config(),
        openqa,
        template,  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
    )


def test_kernel_results_first_export_replaces_placeholder_keeps_notes() -> None:
    """Only the placeholder line is consumed; tester notes on either side
    survive, and the header block has its exact framing blanks."""
    exporter = _kernel(
        [
            "regression tests:\n",
            "my notes\n",
            "(put your details here)\n",
            "more notes\n",
            "\n",
            "build log review:\n",
        ],
        [["r1\n", "r2\n"]],
    )

    exporter.kernel_results()

    tpl = list(exporter.template)
    assert tpl[:2] == ["regression tests:\n", "my notes\n"]
    assert tpl[2].startswith("Results added on ")
    assert tpl[3:] == [
        "\n",
        "Results from openQA:\n",
        "\n",
        "r1\n",
        "r2\n",
        "more notes\n",
        "\n",
        "\n",
        "build log review:\n",
    ]


def test_kernel_results_reexport_bulk_replaces_section_body() -> None:
    """Placeholder gone: everything between 'regression tests:' and
    'build log review:' is replaced with the fresh results."""
    exporter = _kernel(
        [
            "regression tests:\n",
            "Results added on OLD\n",
            "\n",
            "Results from openQA:\n",
            "\n",
            "old-r1\n",
            "stale notes\n",
            "\n",
            "build log review:\n",
        ],
        [["r1\n"]],
    )

    exporter.kernel_results()

    tpl = list(exporter.template)
    assert tpl[0] == "regression tests:\n"
    assert tpl[1].startswith("Results added on ")
    assert "OLD" not in tpl[1]
    assert tpl[2:] == [
        "\n",
        "Results from openQA:\n",
        "\n",
        "r1\n",
        "\n",
        "build log review:\n",
    ]


def test_kernel_results_reexport_uses_legacy_pes_anchor() -> None:
    """With the legacy pes.suse.de line present, deletion starts below it,
    preserving the lines above."""
    exporter = _kernel(
        [
            "regression tests:\n",
            "keep me\n",
            "    * https://pes.suse.de/QA_Maintenance/kernel-default/\n",
            "old results\n",
            "\n",
            "build log review:\n",
        ],
        [["r1\n"]],
    )

    exporter.kernel_results()

    tpl = list(exporter.template)
    assert tpl[:3] == [
        "regression tests:\n",
        "keep me\n",
        "    * https://pes.suse.de/QA_Maintenance/kernel-default/\n",
    ]
    assert tpl[3].startswith("Results added on ")
    assert tpl[4:] == [
        "\n",
        "Results from openQA:\n",
        "\n",
        "r1\n",
        "\n",
        "build log review:\n",
    ]


# ---------------------------------------------------------------------------
# KernelExport.get_logs: the download_logs call contract
# ---------------------------------------------------------------------------


def test_kernel_get_logs_download_contract(tmp_path: Path) -> None:
    cfg = _config(tmp_path)
    cfg.ssl_verify = None  # unset config must still resolve to verifying
    exporter = KernelExport(
        cfg,
        MagicMock(),
        [""],  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
    )
    exporter.openqa = MagicMock()
    exporter.openqa.kernel = []
    in_path = tmp_path / RRID / "install_logs"
    in_path.mkdir(parents=True)
    (in_path / "foo.log").write_text("x")
    res_path = tmp_path / RRID / "results"

    with (
        patch("mtui.update_workflow.export.kernel.download_logs") as dl,
        patch("mtui.update_workflow.export.kernel.ensure_dir_exists") as ensure,
    ):
        out = exporter.get_logs()

    ensure.assert_called_once_with(res_path)
    dl.assert_called_once_with(ANY, res_path, in_path, "tolerant", True)
    assert out == ["foo.log"]


def test_kernel_get_logs_forwards_ssl_verify_override(tmp_path: Path) -> None:
    cfg = _config(tmp_path)
    cfg.ssl_verify = False
    exporter = KernelExport(
        cfg,
        MagicMock(),
        [""],  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
        force=False,
        rrid=RRID,
        interactive=False,
    )
    exporter.openqa = MagicMock()
    exporter.openqa.kernel = []

    with (
        patch("mtui.update_workflow.export.kernel.download_logs") as dl,
        patch("mtui.update_workflow.export.kernel.ensure_dir_exists"),
    ):
        exporter.get_logs()

    assert dl.call_args.args[4] is False
