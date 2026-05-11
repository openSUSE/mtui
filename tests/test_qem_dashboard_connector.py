import responses

from mtui.connector.qem_dashboard import (
    DashboardAutoOpenQA,
    QEMIncident,
)
from mtui.types import RequestReviewID

API = "https://dashboard.example.com/api"
OPENQA_HOST = "https://openqa.example.com"


def _make_dashboard(mock_config) -> DashboardAutoOpenQA:
    """Build a DashboardAutoOpenQA without hitting the API."""
    rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    incident = QEMIncident.__new__(QEMIncident)
    incident.rrid = rrid
    incident.incident_number = 12358
    incident.client = None  # type: ignore[assignment]
    incident.data = {"number": 12358, "packages": ["bash"]}
    return DashboardAutoOpenQA(mock_config, OPENQA_HOST, incident, rrid)


def _incident_job(
    job_id: int,
    name: str,
    result: str,
    *,
    version: str = "15-SP5",
    flavor: str = "Server-DVD-Incidents",
    arch: str = "x86_64",
) -> dict:
    return {
        "id": job_id,
        "test": name,
        "result": result,
        "source": "incident",
        "settings": {
            "DISTRI": "sle",
            "FLAVOR": flavor,
            "ARCH": arch,
            "VERSION": version,
            "BUILD": ":12358:bash",
        },
    }


def _aggregate_job(
    job_id: int,
    name: str,
    result: str,
    *,
    product: str = "SLES-15-SP5",
    build: str = "20240101-1",
    arch: str = "x86_64",
) -> dict:
    return {
        "id": job_id,
        "test": name,
        "result": result,
        "source": "aggregate",
        "product": product,
        "settings": {
            "DISTRI": "sle",
            "FLAVOR": "Server-DVD-Updates",
            "ARCH": arch,
            "VERSION": "15-SP5",
            "BUILD": build,
        },
    }


@responses.activate
def test_qem_incident_metadata():
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={
            "number": 12358,
            "packages": ["kernel-default", "kernel-ec2"],
            "channels": ["SUSE:SLE-12-SP2:Update"],
        },
        status=200,
    )

    incident = QEMIncident(RequestReviewID("SUSE:Maintenance:12358:199773"), API)

    assert incident
    assert incident.get_incident_name() == "kernel-ec2"
    assert incident.get_version() == "12-SP2"


@responses.activate
def test_qem_incident_uses_review_id_for_slfo_1_2(mock_config):
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={"number": 12358, "packages": ["bash"], "channels": []},
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/incident_settings/12358",
        json=[],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/update_settings/12358",
        json=[],
        status=200,
    )

    rrid = RequestReviewID("SUSE:SLFO:1.2:12358")
    incident = QEMIncident(rrid, API)
    DashboardAutoOpenQA(mock_config, "https://openqa.example.com", incident, rrid).run()

    assert incident.incident_number == 12358
    # `incidents/...` runs first in QEMIncident.__init__ (synchronous);
    # the two settings calls fan out concurrently in _load_jobs so their
    # relative order is nondeterministic.
    call_urls = [call.request.url for call in responses.calls]
    assert call_urls[0] == f"{API}/incidents/12358"
    assert sorted(call_urls[1:]) == sorted(
        [
            f"{API}/incident_settings/12358",
            f"{API}/update_settings/12358",
        ]
    )


@responses.activate
def test_dashboard_auto_openqa_loads_incident_and_aggregate_jobs(mock_config):
    rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={"number": 12358, "packages": ["bash"], "channels": []},
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/incident_settings/12358",
        json=[
            {
                "id": 7,
                "incident": 12358,
                "version": "15-SP5",
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "settings": {"DISTRI": "sle"},
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/jobs/incident/7",
        json=[
            {
                "job_id": 1001,
                "incident_settings": 7,
                "update_settings": None,
                "name": "qam-incidentinstall",
                "job_group": "Maintenance",
                "group_id": 1,
                "status": "passed",
                "distri": "sle",
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "version": "15-SP5",
                "build": ":12358:bash",
                "obsolete": False,
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/update_settings/12358",
        json=[
            {
                "id": 23,
                "incidents": [12358],
                "product": "SLES-15-SP5",
                "arch": "x86_64",
                "build": "20240101-1",
                "repohash": "abc123",
                "settings": {"DISTRI": "sle", "VERSION": "15-SP5"},
            }
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/jobs/update/23",
        json=[
            {
                "job_id": 1002,
                "incident_settings": None,
                "update_settings": 23,
                "name": "mau-webserver@64bit",
                "job_group": "Aggregate",
                "group_id": 2,
                "status": "failed",
                "distri": "sle",
                "flavor": "Server-DVD-Updates",
                "arch": "x86_64",
                "version": "15-SP5",
                "build": "20240101-1",
                "obsolete": False,
            }
        ],
        status=200,
    )

    incident = QEMIncident(rrid, API)
    dashboard = DashboardAutoOpenQA(
        mock_config, "https://openqa.example.com", incident, rrid
    ).run()

    assert len(dashboard.jobs) == 2
    assert any("Incident jobs" in line for line in dashboard.pp)
    assert any("Aggregate jobs" in line for line in dashboard.pp)
    assert any("mau-webserver" in line for line in dashboard.pp)
    assert dashboard.results is not None
    assert len(dashboard.results) == 1
    assert dashboard.results[0].url == (
        "https://openqa.example.com/tests/1001/file/install-logs.tar"
    )
    assert dashboard.results[0].result == "passed"


def test_pretty_print_collapses_passed(mock_config):
    dashboard = _make_dashboard(mock_config)
    jobs = [_incident_job(2000 + i, f"qam-test-{i}", "passed") for i in range(50)]
    out = "".join(dashboard._pretty_print(jobs))

    # All 50 jobs collapsed into a single summary row.
    assert "Summary:" in out
    assert "passed: 50" in out
    assert "total: 50" in out
    # Zero counters MUST NOT appear in the condensed Summary.
    for noisy in (
        "softfailed: 0",
        "failed: 0",
        "incomplete: 0",
        "timeout_exceeded: 0",
        "other: 0",
    ):
        assert noisy not in out
    # No individual passed job names should be present.
    for i in range(50):
        assert f"qam-test-{i}" not in out
    # No "Failed jobs" subsection because nothing failed.
    assert "Failed jobs:" not in out
    assert "All jobs passed." in out


def test_pretty_print_lists_failed(mock_config):
    dashboard = _make_dashboard(mock_config)
    jobs = [_incident_job(3000 + i, f"qam-pass-{i}", "passed") for i in range(10)] + [
        _incident_job(3100, "qam-failure", "failed"),
        _incident_job(3101, "qam-incomplete", "incomplete"),
        _incident_job(3102, "qam-timeout", "timeout_exceeded"),
        _incident_job(3103, "qam-soft", "softfailed"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    # Summary counts: only non-zero counters appear.
    assert "passed: 10" in out
    assert "softfailed: 1" in out
    assert "failed: 1" in out
    assert "incomplete: 1" in out
    assert "timeout_exceeded: 1" in out
    assert "total: 14" in out
    # Zero counters MUST NOT appear.
    assert "other: 0" not in out

    # Only failed/incomplete/timeout_exceeded jobs are listed individually.
    assert "Failed jobs:" in out
    # Group header in the new nested layout.
    assert "15-SP5 / Server-DVD-Incidents / x86_64 (3 failed):" in out
    assert "qam-failure" in out
    assert "qam-incomplete" in out
    assert "qam-timeout" in out
    # Non-failed results are tagged with [result] inside the group.
    assert "[incomplete]" in out
    assert "[timeout_exceeded]" in out
    # Passed and softfailed jobs do NOT appear individually.
    assert "qam-soft" not in out
    for i in range(10):
        assert f"qam-pass-{i}" not in out

    # Failed-job lines include the openQA URL.
    assert f"{OPENQA_HOST}/tests/3100" in out
    assert f"{OPENQA_HOST}/tests/3101" in out
    assert f"{OPENQA_HOST}/tests/3102" in out


def test_pretty_print_aggregate_grouping(mock_config):
    dashboard = _make_dashboard(mock_config)
    jobs = [
        _aggregate_job(4000, "mau-a", "passed", product="SLES-15-SP5"),
        _aggregate_job(4001, "mau-b", "passed", product="SLES-15-SP5"),
        _aggregate_job(4002, "mau-c", "failed", product="SLES-15-SP6"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    assert "Aggregate jobs:" in out
    # Shared BUILD is hoisted, not repeated per-row.
    assert "  build: 20240101-1\n" in out
    # Two distinct product groups, each with its own summary row.
    assert "product: SLES-15-SP5" in out
    assert "product: SLES-15-SP6" in out
    # `build:` MUST NOT appear in any group / failed-job header.
    assert " - build: " not in out
    # SP6 (problem group) appears before the folded SP5 (all-passed).
    assert out.index("SLES-15-SP6") < out.index("SLES-15-SP5")
    # SP5 group has both passed jobs collapsed.
    assert "passed: 2" in out
    # SP6 group's failed job appears individually under a nested header.
    assert "Failed jobs:" in out
    assert "SLES-15-SP6 / x86_64 (1 failed):" in out
    assert "mau-c" in out
    # Passed aggregate jobs are not individually listed.
    assert "mau-a" not in out
    assert "mau-b" not in out


def test_pretty_print_unknown_grouping_keys(mock_config):
    dashboard = _make_dashboard(mock_config)
    job = _incident_job(5000, "qam-x", "passed")
    job["settings"]["VERSION"] = None
    job["settings"]["FLAVOR"] = ""
    out = "".join(dashboard._pretty_print([job]))

    assert "version: unknown" in out
    assert "flavor: unknown" in out


def test_format_counts_skips_zeros(mock_config):
    dashboard = _make_dashboard(mock_config)
    counts = dashboard._empty_counts()
    counts["passed"] = 10
    counts["failed"] = 2
    counts["total"] = 12
    out = dashboard._format_counts(counts)

    assert out == "passed: 10, failed: 2, total: 12"
    assert "softfailed" not in out
    assert "other" not in out


def test_pretty_print_aggregate_hoists_build(mock_config):
    """Single shared BUILD is hoisted; mixed builds keep per-row build."""
    dashboard = _make_dashboard(mock_config)

    # Single-build section: build is hoisted and absent from rows.
    same_build = [
        _aggregate_job(6000, "mau-x", "passed", product="A", build="20260101-1"),
        _aggregate_job(6001, "mau-y", "passed", product="B", build="20260101-1"),
    ]
    out_same = "".join(dashboard._pretty_print(same_build))
    assert "  build: 20260101-1\n" in out_same
    assert " - build: " not in out_same

    # Mixed builds: no hoist; per-row build re-appears.
    mixed_build = [
        _aggregate_job(6100, "mau-x", "passed", product="A", build="20260101-1"),
        _aggregate_job(6101, "mau-y", "passed", product="B", build="20260102-2"),
    ]
    out_mixed = "".join(dashboard._pretty_print(mixed_build))
    # Hoist line absent.
    assert "  build: 20260101-1\n" not in out_mixed
    assert "  build: 20260102-2\n" not in out_mixed
    # Per-row build present on at least one summary row (folded/full).
    assert "build: 20260101-1" in out_mixed
    assert "build: 20260102-2" in out_mixed


def test_pretty_print_problem_groups_sorted_first(mock_config):
    dashboard = _make_dashboard(mock_config)
    jobs = [
        # All-passed group inserted first.
        _incident_job(7000, "qam-ok-1", "passed", version="15-SP4"),
        _incident_job(7001, "qam-ok-2", "passed", version="15-SP4"),
        # Problem group inserted second.
        _incident_job(7100, "qam-bad", "failed", version="15-SP7"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    summary_start = out.index("Summary:")
    failed_start = out.index("Failed jobs:")
    summary_block = out[summary_start:failed_start]
    # Problem group (15-SP7) appears before all-passed group (15-SP4).
    assert summary_block.index("15-SP7") < summary_block.index("15-SP4")


def test_pretty_print_folds_all_passed_archs(mock_config):
    """All-passed groups across archs collapse into one folded row."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        _incident_job(8000, "t1", "passed", arch="x86_64"),
        _incident_job(8001, "t2", "passed", arch="x86_64"),
        _incident_job(8002, "t3", "passed", arch="aarch64"),
        _incident_job(8003, "t4", "passed", arch="s390x"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    # One folded row instead of three per-arch rows.
    assert "archs: x86_64, aarch64, s390x" in out
    assert "passed: 4" in out
    assert "total: 4" in out
    assert "(3 arches)" in out
    # No per-arch rows for the folded group.
    assert "arch: x86_64" not in out
    assert "arch: aarch64" not in out
    assert "arch: s390x" not in out


def test_pretty_print_does_not_fold_problem_groups(mock_config):
    """A group with a failure stays per-arch so reviewers see which arch broke."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        _incident_job(9000, "t-pass", "passed", arch="x86_64"),
        _incident_job(9001, "t-fail", "failed", arch="aarch64"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    # Problem group keeps its per-arch row (no fold), folded passed group also present.
    assert "arch: aarch64" in out
    # The all-passed x86_64 group is folded (no archs: line for it because
    # it's the only arch in the fold, but `arch: x86_64` MUST NOT appear
    # as a per-arch summary row).
    assert "arch: x86_64" not in out
    assert "archs: x86_64" in out


def test_pretty_print_failed_jobs_grouped(mock_config):
    """Failed-jobs subsection nests under group header, drops redundant prefix."""
    dashboard = _make_dashboard(mock_config)
    jobs = [
        _aggregate_job(10000, "test-alpha", "failed", product="P", arch="x86_64"),
        _aggregate_job(10001, "test-beta-longer", "failed", product="P", arch="x86_64"),
    ]
    out = "".join(dashboard._pretty_print(jobs))

    # Group header line with hoisted build (single build in section).
    assert "P / x86_64 (2 failed):" in out
    # Tests are listed indented under the header; URLs aligned via padding.
    lines = [
        line
        for line in out.splitlines()
        if "test-alpha" in line or "test-beta-longer" in line
    ]
    assert len(lines) == 2
    # The shorter test name is padded to the longer one's width, so the URL
    # column starts at the same offset on both rows.
    url_offsets = [line.index(OPENQA_HOST) for line in lines]
    assert url_offsets[0] == url_offsets[1]
    # No `product:` / `build:` / `arch:` repeated on a per-failure line.
    assert "product: P - " not in out.split("Failed jobs:")[1]


@responses.activate
def test_dashboard_auto_openqa_fans_out_per_setting_fetches(mock_config):
    """Per-setting jobs fetches run concurrently; each URL hits exactly once
    and the resulting jobs list preserves insertion order (incident settings
    in order, then update settings in order).
    """
    rrid = RequestReviewID("SUSE:Maintenance:12358:199773")
    responses.add(
        responses.GET,
        f"{API}/incidents/12358",
        json={"number": 12358, "packages": ["bash"], "channels": []},
        status=200,
    )

    incident_setting_ids = [11, 12, 13]
    update_setting_ids = [21, 22, 23]

    responses.add(
        responses.GET,
        f"{API}/incident_settings/12358",
        json=[
            {
                "id": sid,
                "incident": 12358,
                "version": "15-SP5",
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "settings": {"DISTRI": "sle"},
            }
            for sid in incident_setting_ids
        ],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{API}/update_settings/12358",
        json=[
            {
                "id": sid,
                "incidents": [12358],
                "product": "SLES-15-SP5",
                "arch": "x86_64",
                "build": "20240101-1",
                "repohash": "abc123",
                "settings": {"DISTRI": "sle", "VERSION": "15-SP5"},
            }
            for sid in update_setting_ids
        ],
        status=200,
    )

    # One job per setting, named so we can verify ordering.
    for sid in incident_setting_ids:
        responses.add(
            responses.GET,
            f"{API}/jobs/incident/{sid}",
            json=[
                {
                    "job_id": 1000 + sid,
                    "name": f"qam-incident-{sid}",
                    "status": "passed",
                }
            ],
            status=200,
        )
    for sid in update_setting_ids:
        responses.add(
            responses.GET,
            f"{API}/jobs/update/{sid}",
            json=[
                {
                    "job_id": 2000 + sid,
                    "name": f"mau-update-{sid}",
                    "status": "passed",
                }
            ],
            status=200,
        )

    incident = QEMIncident(rrid, API)
    dashboard = DashboardAutoOpenQA(
        mock_config, "https://openqa.example.com", incident, rrid
    ).run()

    # Each per-setting URL is called exactly once (no duplicates from the
    # concurrent fan-out).
    call_urls = [call.request.url for call in responses.calls]
    for sid in incident_setting_ids:
        assert call_urls.count(f"{API}/jobs/incident/{sid}") == 1
    for sid in update_setting_ids:
        assert call_urls.count(f"{API}/jobs/update/{sid}") == 1

    # Insertion order is preserved: incident jobs (in setting order) come
    # before aggregate jobs (in setting order). This is what the
    # pretty-printer relies on to keep summaries stable.
    assert [job["test"] for job in dashboard.jobs] == [
        f"qam-incident-{sid}" for sid in incident_setting_ids
    ] + [f"mau-update-{sid}" for sid in update_setting_ids]
