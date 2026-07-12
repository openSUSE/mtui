"""Mutation-killing pinning tests for ``mtui.data_sources.oqa_search``.

A full mutmut run left survivors in code the suite executes but never
asserts on. These tests pin the exact current behavior of the search
module (and its HTTP helpers) so those mutants die:

- ``incident_jobs``: dict-key fallbacks, the ``group`` field, and the
  obsoleted-job ``continue`` (not ``break``).
- ``_scan_aggregated_for_version``: the day-walk keeps going backward
  past per-day errors and queries strictly older build dates.
- ``_query_version_status`` / ``_render_version_row``: the third
  RUNNING state, which no existing test exercised.
- ``single_incidents`` / ``aggregated_updates``: error rows for HTTP
  failures and the per-item ``continue`` on invalid versions/groups.
- ``render_overview`` / ``_render_build_check``: golden line-by-line
  output instead of substring checks.
- ``get_incident_info``: first-entry BUILD selection and the
  ``_fallback_build`` except path.
- ``build_checks``: per-log HTTP failure fallback and the exact fold
  boundary (>4) plus folded first/last content.
- ``extract_test_results``: IGNORECASE on custom patterns and exact
  matched-line content on the visual-separator branch.
- ``_get_json`` / ``_fetch_url_content``: bounded timeout and the
  ``_HTTPError`` message carrying the cause text.
"""

from __future__ import annotations

from datetime import datetime, timedelta
from urllib.parse import parse_qs, urlparse

import pytest
import responses

from mtui.data_sources import oqa_search
from mtui.data_sources.oqa_search import http as oqa_http
from mtui.data_sources.oqa_search.results import (
    BuildCheckResult,
    GroupResult,
    JobResult,
    VersionResult,
)
from mtui.support.http import HTTP_TIMEOUT

OPENQA = "https://openqa.example.com"
DASHBOARD = "https://dashboard.example.com"
QAM = "https://qam.example.com"


@pytest.fixture(autouse=True)
def _clear_caches():
    """Drop the job-group lru_cache before and after every test."""
    oqa_search._fetch_openqa_groups.cache_clear()
    yield
    oqa_search._fetch_openqa_groups.cache_clear()


def _register_job_groups(*groups: dict) -> None:
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/job_groups",
        json=list(groups),
        status=200,
    )


def _core_incidents_group() -> dict:
    return {"id": 490, "name": "SLE 15 SP5 Core Incidents", "template": "tpl"}


def _core_aggregated_group() -> dict:
    return {"id": 367, "name": "Core Maintenance Updates 15-SP5", "template": "tpl"}


def _build_param(call) -> str:
    """Extract the ``build`` query parameter from a recorded request."""
    return parse_qs(urlparse(call.request.url).query)["build"][0]


def _date_candidates(t0: datetime, t1: datetime, days_back: int) -> set[str]:
    """Acceptable ``YYYYMMDD-1`` builds for ``days_back`` days before now.

    Computed from timestamps taken just before and just after the call
    under test so a midnight rollover mid-test cannot flake.
    """
    return {f"{(t - timedelta(days=days_back)).strftime('%Y%m%d')}-1" for t in (t0, t1)}


# --- incident_jobs -----------------------------------------------------------


@responses.activate
def test_incident_jobs_obsoleted_in_middle_does_not_stop_the_loop():
    """An obsoleted job mid-list is skipped; later jobs still appear."""
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                {"id": 1, "test": "a_first", "result": "passed", "settings": {}},
                {"id": 2, "test": "b_stale", "result": "obsoleted", "settings": {}},
                {"id": 3, "test": "c_last", "result": "failed", "settings": {}},
            ]
        },
        status=200,
    )

    rows = oqa_search.incident_jobs(":12358:bash", OPENQA)

    # The job AFTER the obsoleted one survives (continue, not break).
    assert [r.test for r in rows] == ["c_last", "a_first"]  # sorted by result


@responses.activate
def test_incident_jobs_pins_every_field_and_fallback_defaults():
    """Each JobResult field maps from the documented job key with its default."""
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                # Fully populated job: pins the happy-path key mapping,
                # including the never-before-asserted `group` field.
                {
                    "id": 7,
                    "test": "fips_smoke",
                    "result": "passed",
                    "group": "Maintenance: Core",
                    "state": "done",
                    "settings": {"ARCH": "s390x"},
                },
                # Empty job: pins every .get() default.
                {},
            ]
        },
        status=200,
    )

    rows = oqa_search.incident_jobs(":12358:bash", OPENQA)

    assert rows == [
        JobResult(
            job_id=0,
            test="",
            arch="",
            result="",
            group="",
            url=f"{OPENQA}/t",
            state="",
        ),
        JobResult(
            job_id=7,
            test="fips_smoke",
            arch="s390x",
            result="passed",
            group="Maintenance: Core",
            url=f"{OPENQA}/t7",
            state="done",
        ),
    ]


@responses.activate
def test_incident_jobs_arch_falls_back_to_job_key_and_name():
    """ARCH prefers settings, falls back to the job's arch; test falls back to name."""
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                {
                    "id": 9,
                    "name": "from_name",
                    "result": "passed",
                    "arch": "aarch64",
                    "settings": {},
                }
            ]
        },
        status=200,
    )

    rows = oqa_search.incident_jobs(":12358:bash", OPENQA)

    assert rows[0].test == "from_name"
    assert rows[0].arch == "aarch64"


# --- _query_version_status: the RUNNING state --------------------------------


@responses.activate
def test_single_incidents_running_status():
    """No failed jobs + scheduled/running jobs -> RUNNING with a count."""
    _register_job_groups(_core_incidents_group())
    responses.add(  # running query comes first in _query_version_status
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 1}, {"id": 2}],
        status=200,
    )
    responses.add(  # failed query
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["15-SP5"], OPENQA)

    assert rows == [
        VersionResult(
            version="15-SP5",
            url=(
                f"{OPENQA}/tests/overview"
                "?distri=sle&version=15-SP5&build=:12358:bash&groupid=490"
            ),
            status="running",
            running_count=2,
        )
    ]
    # The first overview request carried the running-state filter.
    running_request_url = responses.calls[1].request.url or ""
    assert "state=scheduled" in running_request_url
    assert "state=running" in running_request_url


@responses.activate
def test_single_incidents_failed_beats_running():
    """Failed jobs win over running ones in the status resolution."""
    _register_job_groups(_core_incidents_group())
    responses.add(  # running: non-empty
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 1}],
        status=200,
    )
    responses.add(  # failed: non-empty
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 2}, {"id": 3}],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["15-SP5"], OPENQA)

    assert rows[0].status == "failed"
    assert rows[0].failed_count == 2
    assert rows[0].running_count == 0


# --- single_incidents error paths ---------------------------------------------


@responses.activate
def test_single_incidents_unknown_version_continues_to_next_version():
    """An unrecognized version yields a failed row but does not stop the loop."""
    _register_job_groups(_core_incidents_group())
    responses.add(  # running for 15-SP5
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed for 15-SP5
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["99-SP99", "15-SP5"], OPENQA)

    assert len(rows) == 2
    assert rows[0].version == "99-SP99"
    assert rows[0].status == "failed"
    assert "99-SP99" in rows[0].note
    # The version AFTER the bad one is still resolved (continue, not break).
    assert rows[1].version == "15-SP5"
    assert rows[1].status == "passed"


@responses.activate
def test_single_incidents_http_error_becomes_failed_row_with_note():
    """A 500 from openQA converts into a failed row with a query-failed note."""
    _register_job_groups(_core_incidents_group())
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        status=500,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["15-SP5"], OPENQA)

    assert len(rows) == 1
    assert rows[0].version == "15-SP5"
    assert rows[0].status == "failed"
    assert rows[0].url == ""
    assert rows[0].note.startswith("openQA query failed: ")
    assert "500" in rows[0].note


# --- _scan_aggregated_for_version day-walk ------------------------------------


@responses.activate
def test_scan_aggregated_day_error_walks_back_to_previous_day():
    """A failing day-0 query is skipped; day 1 (yesterday) is still tried."""
    responses.add(  # day 0 "all" query errors
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        status=500,
    )
    responses.add(  # day 1 "all" query finds a job
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 999}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/999",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358"}}},
        status=200,
    )
    responses.add(  # running
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    t0 = datetime.now()
    row = oqa_search._scan_aggregated_for_version(OPENQA, "15-SP5", 2, 367, 12358)
    t1 = datetime.now()

    assert row.status == "passed"
    # The day-walk queried today first, then strictly one day earlier --
    # not the same day again and not a day in the future.
    assert _build_param(responses.calls[0]) in _date_candidates(t0, t1, 0)
    assert _build_param(responses.calls[1]) in _date_candidates(t0, t1, 1)
    # The result URL points at yesterday's build.
    assert any(b in row.url for b in _date_candidates(t0, t1, 1))


@responses.activate
def test_scan_aggregated_missing_job_id_walks_back_to_previous_day():
    """A day whose first job has no id is skipped, not the end of the walk."""
    responses.add(  # day 0: job without an id
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"name": "no-id-here"}],
        status=200,
    )
    responses.add(  # day 1: usable job
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 555}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/555",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358"}}},
        status=200,
    )
    responses.add(  # running
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    t0 = datetime.now()
    row = oqa_search._scan_aggregated_for_version(OPENQA, "15-SP5", 2, 367, 12358)
    t1 = datetime.now()

    assert row.status == "passed"
    assert any(b in row.url for b in _date_candidates(t0, t1, 1))


@responses.activate
def test_scan_aggregated_job_issues_error_walks_back_to_previous_day():
    """A failing job-issues lookup skips the day instead of aborting."""
    responses.add(  # day 0: job exists ...
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 111}],
        status=200,
    )
    responses.add(  # ... but its issues lookup fails
        responses.GET,
        f"{OPENQA}/api/v1/jobs/111",
        status=500,
    )
    responses.add(  # day 1: usable job
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 222}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/222",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358"}}},
        status=200,
    )
    responses.add(  # running
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    t0 = datetime.now()
    row = oqa_search._scan_aggregated_for_version(OPENQA, "15-SP5", 2, 367, 12358)
    t1 = datetime.now()

    assert row.status == "passed"
    assert any(b in row.url for b in _date_candidates(t0, t1, 1))


@responses.activate
def test_scan_aggregated_status_query_error_returns_failed_row():
    """An _HTTPError from the status resolution becomes a failed row + note."""
    responses.add(  # day 0 "all": a matching job
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 999}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/999",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358"}}},
        status=200,
    )
    responses.add(  # running query inside _query_version_status errors
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        status=500,
    )

    row = oqa_search._scan_aggregated_for_version(OPENQA, "15-SP5", 2, 367, 12358)

    assert row.version == "15-SP5"
    assert row.status == "failed"
    assert row.url == ""
    assert row.note.startswith("openQA query failed: ")


# --- aggregated_updates --------------------------------------------------------


@responses.activate
def test_aggregated_updates_invalid_group_continues_to_next_group():
    """An unknown group is skipped; the next group still produces results."""
    _register_job_groups(_core_aggregated_group())
    responses.add(  # core, day 0: no jobs -> missing row after the window
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    out = oqa_search.aggregated_updates(
        12358, ["15-SP5"], 1, ["bogus-group", "core"], OPENQA
    )

    assert len(out) == 1
    assert out[0].group == "core"
    assert out[0].versions[0].status == "missing"


@responses.activate
def test_aggregated_updates_incident_filter_skips_non_matching_days():
    """A day whose jobs test other incidents is skipped, so the filter
    (and the int() conversion feeding it) actually discriminates."""
    _register_job_groups(_core_aggregated_group())
    responses.add(  # day 0: job testing a DIFFERENT incident
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 111}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/111",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "99999"}}},
        status=200,
    )
    responses.add(  # day 1: job covering OUR incident
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 222}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/222",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358,777"}}},
        status=200,
    )
    responses.add(  # running
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    t0 = datetime.now()
    # Pass the incident id as a string to pin the int() conversion.
    out = oqa_search.aggregated_updates("12358", ["15-SP5"], 2, ["core"], OPENQA)
    t1 = datetime.now()

    row = out[0].versions[0]
    assert row.status == "passed"
    # Day 0 did NOT match (its issues lack 12358): the result is day 1's build.
    assert any(b in row.url for b in _date_candidates(t0, t1, 1))


# --- render_overview golden output ---------------------------------------------


def test_render_overview_golden_full_sections():
    """Exact line-by-line output: headers, blank lines, rows, build checks."""
    single = [
        VersionResult(version="15-SP5", url="https://oqa/u1", status="passed"),
        VersionResult(
            version="15-SP4", url="https://oqa/u2", status="failed", failed_count=3
        ),
    ]
    aggregated = [
        GroupResult(
            group="core",
            versions=[
                VersionResult(version="15-SP5", url="https://oqa/agg", status="passed")
            ],
        )
    ]
    build_checks = [
        BuildCheckResult(
            url="https://qam/xz.log",
            matches=["[   28s] All 9 tests passed"],
        )
    ]

    assert oqa_search.render_overview(single, aggregated, build_checks) == [
        "## OpenQA Overview",
        "",
        "### Single Incidents - Core",
        "",
        "- 15-SP5 -> https://oqa/u1",
        "  - PASSED",
        "- 15-SP4 -> https://oqa/u2",
        "  - FAILED (3 jobs)",
        "",
        "### Aggregated Updates - Core",
        "",
        "- 15-SP5 -> https://oqa/agg",
        "  - PASSED",
        "",
        "### Build Checks",
        "",
        "- https://qam/xz.log",
        "  - All 9 tests passed",
        "",
    ]


def test_render_overview_golden_empty_input():
    """Exact placeholder output when there is nothing to show."""
    assert oqa_search.render_overview([], [], []) == [
        "## OpenQA Overview",
        "",
        "_No openQA builds for this incident yet._",
        "",
        "### Build Checks",
        "",
        "_No build checks for this incident._",
        "",
    ]


# --- _render_version_row branch matrix ------------------------------------------


@pytest.mark.parametrize(
    ("row", "expected"),
    [
        (
            VersionResult(
                version="15-SP5", url="https://u", status="running", running_count=4
            ),
            ["- 15-SP5 -> https://u", "  - RUNNING/SCHEDULED (4 jobs)"],
        ),
        (
            VersionResult(version="15-SP5", url="https://u", status="running"),
            ["- 15-SP5 -> https://u", "  - RUNNING/SCHEDULED"],
        ),
        (
            VersionResult(version="15-SP5", url="", status="missing", note="no build"),
            ["- 15-SP5: no build"],
        ),
        (
            VersionResult(version="15-SP5", url="https://u", status="failed"),
            ["- 15-SP5 -> https://u", "  - FAILED"],
        ),
        (
            VersionResult(
                version="15-SP5",
                url="https://u",
                status="failed",
                failed_count=2,
                note="flaky",
            ),
            ["- 15-SP5 -> https://u", "  - FAILED (2 jobs)", "  - note: flaky"],
        ),
        (
            VersionResult(version="15-SP5", url="", status="passed"),
            ["- 15-SP5", "  - PASSED"],
        ),
    ],
)
def test_render_version_row_branches(row, expected):
    assert oqa_search._render_version_row(row) == expected


# --- _render_build_check folded branch -------------------------------------------


def test_render_build_check_folded_summary_order():
    """Folded entries emit first-match, summary, last-match in that order."""
    entry = BuildCheckResult(
        url="https://q/x.log",
        matches=["[   1s] first summary line", "[   9s] last summary line"],
        summary="(3 more results, 5 passed, 1 failed)",
    )
    assert oqa_search._render_build_check(entry) == [
        "- https://q/x.log",
        "  - first summary line",
        "  - (3 more results, 5 passed, 1 failed)",
        "  - last summary line",
        "",
    ]


def test_render_build_check_no_matches_hint():
    assert oqa_search._render_build_check(BuildCheckResult(url="https://q/y.log")) == [
        "- https://q/y.log",
        "  - No test results found (try using a custom pattern with --test-pattern)",
        "",
    ]


# --- get_incident_info -------------------------------------------------------------


@responses.activate
def test_get_incident_info_uses_first_entry_build_and_filters_versions():
    """BUILD comes from entry 0; only sle-DISTRI entries contribute versions."""
    responses.add(
        responses.GET,
        f"{DASHBOARD}/api/incident_settings/12358",
        json=[
            # Entry 0 (its BUILD wins); no "flavor" key at all.
            {
                "settings": {"BUILD": ":12358:bash-first", "DISTRI": "sle"},
                "version": "15-SP5",
            },
            # Different BUILD: must NOT be picked.
            {
                "settings": {"BUILD": ":12358:bash-second", "DISTRI": "sle"},
                "version": "12-SP3",
                "flavor": "Server-TERADATA",
            },
            # Non-sle DISTRI: excluded from versions.
            {
                "settings": {"BUILD": "x", "DISTRI": "opensuse"},
                "version": "16.0",
                "flavor": "X",
            },
            # No "settings" key: excluded from versions, must not crash.
            {"version": "15-SP9", "flavor": "Y"},
        ],
        status=200,
    )

    build, versions = oqa_search.get_incident_info(DASHBOARD, 12358)

    assert build == ":12358:bash-first"
    assert versions == ["12-SP3-TERADATA", "15-SP5"]


@responses.activate
def test_get_incident_info_missing_build_key_falls_back():
    """Entry 0 without settings.BUILD triggers the /incidents fallback."""
    responses.add(
        responses.GET,
        f"{DASHBOARD}/api/incident_settings/12358",
        json=[{"settings": {"DISTRI": "sle"}, "version": "15-SP5"}],
        status=200,
    )
    responses.add(
        responses.GET,
        f"{DASHBOARD}/api/incidents/12358",
        json={"packages": ["bash"]},
        status=200,
    )

    build, versions = oqa_search.get_incident_info(DASHBOARD, 12358)

    assert build == ":12358:bash"
    assert versions is None


# --- build_checks -------------------------------------------------------------------

_BC_BASE = f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks"


@responses.activate
def test_build_checks_per_log_error_yields_bare_result_and_continues():
    """A single failing log becomes a bare entry; later logs still parse."""
    responses.add(
        responses.GET,
        _BC_BASE,
        body='<a href="bash.one.log">1</a><a href="bash.two.log">2</a>',
        status=200,
        content_type="text/html",
    )
    responses.add(responses.GET, f"{_BC_BASE}/bash.one.log", status=500)
    responses.add(
        responses.GET,
        f"{_BC_BASE}/bash.two.log",
        body="3 widgets",
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ["bash"], QAM, r"\d+ widgets"
    )

    assert out == [
        BuildCheckResult(url=f"{_BC_BASE}/bash.one.log", matches=[], summary=""),
        BuildCheckResult(
            url=f"{_BC_BASE}/bash.two.log", matches=["3 widgets"], summary=""
        ),
    ]


@responses.activate
def test_build_checks_exactly_four_matches_not_folded():
    """The fold triggers strictly above four matches; four stay verbatim."""
    responses.add(
        responses.GET,
        _BC_BASE,
        body='<a href="bash.x86_64.log">x</a>',
        status=200,
        content_type="text/html",
    )
    responses.add(
        responses.GET,
        f"{_BC_BASE}/bash.x86_64.log",
        body="1 widgets\n2 widgets\n3 widgets\n4 widgets",
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ["bash"], QAM, r"\d+ widgets"
    )

    assert len(out) == 1
    assert out[0].summary == ""
    assert out[0].matches == ["1 widgets", "2 widgets", "3 widgets", "4 widgets"]


@responses.activate
def test_build_checks_fold_keeps_true_first_and_last_lines():
    """Folded matches are exactly the first and last matched lines."""
    responses.add(
        responses.GET,
        _BC_BASE,
        body='<a href="bash.x86_64.log">x</a>',
        status=200,
        content_type="text/html",
    )
    responses.add(
        responses.GET,
        f"{_BC_BASE}/bash.x86_64.log",
        body="1 widgets\n2 widgets\n3 widgets\n4 widgets\n5 widgets",
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ["bash"], QAM, r"\d+ widgets"
    )

    assert len(out) == 1
    assert out[0].matches == ["1 widgets", "5 widgets"]
    assert out[0].summary == "(3 more results, 0 passed, 0 failed)"


# --- extract_test_results -------------------------------------------------------------


def test_extract_test_results_custom_pattern_is_case_insensitive():
    """The user's pattern matches regardless of case (re.IGNORECASE)."""
    out = oqa_search.extract_test_results("FOO 3 WIDGETS", r"\d+ widgets")
    assert out == ["FOO 3 WIDGETS"]


def test_extract_test_results_separator_lines_kept_verbatim():
    """Visual-separator lines are appended as-is (original line, in order)."""
    log = "\n".join(
        [
            "[   12s] === 5 tests passed ===",
            "[   13s] just some chatter",
            "[   14s] === 3 tests total ===",
        ]
    )
    assert oqa_search.extract_test_results(log) == [
        "[   12s] === 5 tests passed ===",
        "[   14s] === 3 tests total ===",
    ]


# --- HTTP helpers: _get_json / _fetch_url_content ---------------------------------------


@responses.activate
def test_get_json_http_status_error_raises_with_cause_text():
    responses.add(responses.GET, f"{OPENQA}/api/v1/boom", status=500)
    with pytest.raises(oqa_http._HTTPError, match="500 Server Error"):
        oqa_http._get_json(f"{OPENQA}/api/v1/boom")


@responses.activate
def test_get_json_invalid_json_raises_with_cause_text():
    responses.add(
        responses.GET, f"{OPENQA}/api/v1/notjson", body="not json", status=200
    )
    with pytest.raises(oqa_http._HTTPError, match="Expecting value"):
        oqa_http._get_json(f"{OPENQA}/api/v1/notjson")


@responses.activate
def test_fetch_url_content_http_status_error_raises_with_cause_text():
    responses.add(responses.GET, f"{QAM}/gone", status=500)
    with pytest.raises(oqa_http._HTTPError, match="500 Server Error"):
        oqa_http._fetch_url_content(f"{QAM}/gone")


class _CapturingSession:
    """Fake session recording the kwargs of the single get() call."""

    def __init__(self, payload: str) -> None:
        self.payload = payload
        self.captured: dict = {}

    def get(self, url, **kwargs):
        self.captured = {"url": url, **kwargs}
        session = self

        class _Resp:
            text = session.payload

            @staticmethod
            def raise_for_status() -> None:
                return None

            @staticmethod
            def json() -> dict:
                return {"ok": True}

        return _Resp()


def test_get_json_passes_bounded_timeout(monkeypatch):
    fake = _CapturingSession('{"ok": true}')
    monkeypatch.setattr(oqa_http, "_session", lambda: fake)

    assert oqa_http._get_json(f"{OPENQA}/api/v1/x") == {"ok": True}
    assert fake.captured["timeout"] == HTTP_TIMEOUT


def test_fetch_url_content_passes_bounded_timeout(monkeypatch):
    fake = _CapturingSession("hello")
    monkeypatch.setattr(oqa_http, "_session", lambda: fake)

    assert oqa_http._fetch_url_content(f"{QAM}/index") == "hello"
    assert fake.captured["timeout"] == HTTP_TIMEOUT
