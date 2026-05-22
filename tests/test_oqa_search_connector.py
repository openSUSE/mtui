"""Tests for ``mtui.connector.oqa_search``.

Covers the three public entry points (`single_incidents`,
`aggregated_updates`, `build_checks`) with `responses`-mocked HTTP.
The shared `lru_cache` on `_fetch_openqa_groups` is cleared between
tests so each test sees a clean slate.
"""

from __future__ import annotations

from datetime import datetime

import pytest
import responses

from mtui.connector import oqa_search

OPENQA = "https://openqa.example.com"
DASHBOARD = "https://dashboard.example.com"
QAM = "https://qam.example.com"


@pytest.fixture(autouse=True)
def _clear_caches():
    """Drop the lru_cache before and after every test."""
    oqa_search._fetch_openqa_groups.cache_clear()
    yield
    oqa_search._fetch_openqa_groups.cache_clear()


def _job_group(group_id: int, name: str, template: str = "tpl") -> dict:
    return {"id": group_id, "name": name, "template": template}


def _register_job_groups(*groups: dict) -> None:
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/job_groups",
        json=list(groups),
        status=200,
    )


# --- _parse_update_id-equivalent: verify RequestReviewID-shaped flow ---


def test_extract_version_handles_all_three_forms():
    assert oqa_search._extract_version("Maintenance: 12-SP5") == "12-SP5"
    assert (
        oqa_search._extract_version("Maintenance: 12 SP3 TERADATA") == "12-SP3-TERADATA"
    )
    assert (
        oqa_search._extract_version("Maintenance: 15-SP4-TERADATA") == "15-SP4-TERADATA"
    )
    assert oqa_search._extract_version("SLES 16.0 Maintenance Updates") == "16.0"
    assert oqa_search._extract_version("no version here") == ""


def test_extract_aggregated_name_maps_and_falls_back():
    assert (
        oqa_search._extract_aggregated_name("Public Cloud Maintenance Updates")
        == "cloud"
    )
    assert oqa_search._extract_aggregated_name("SAP/HA Maintenance Updates") == "sap"
    assert (
        oqa_search._extract_aggregated_name("Core Maintenance Updates 15-SP5") == "core"
    )


# --- single_incidents happy path ---


@responses.activate
def test_single_incidents_passed():
    """A SLE version with no failed and no running jobs reports PASSED."""
    _register_job_groups(
        _job_group(490, "SLE 15 SP5 Core Incidents"),
        _job_group(521, "SLE 12 SP4 TERADATA Core Incidents"),
    )
    # both running and failed return empty -> PASSED
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["15-SP5"], OPENQA)

    assert len(rows) == 1
    assert rows[0].version == "15-SP5"
    assert rows[0].status == "passed"
    assert rows[0].url.startswith(f"{OPENQA}/tests/overview")
    assert "groupid=490" in rows[0].url


@responses.activate
def test_single_incidents_failed_counts_jobs():
    """A version with failed jobs reports FAILED with the count populated."""
    _register_job_groups(_job_group(490, "SLE 15 SP5 Core Incidents"))

    # the connector hits the same /jobs/overview endpoint twice (running, failed);
    # `responses` matches them in registration order.
    responses.add(  # running
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )
    responses.add(  # failed
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 1}, {"id": 2}, {"id": 3}],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["15-SP5"], OPENQA)
    assert rows[0].status == "failed"
    assert rows[0].failed_count == 3


@responses.activate
def test_single_incidents_unknown_version_records_note():
    """Versions not in the live group map become a 'failed' row with a note."""
    _register_job_groups(_job_group(490, "SLE 15 SP5 Core Incidents"))

    rows = oqa_search.single_incidents(":12358:bash", ["99-SP99"], OPENQA)
    assert rows[0].status == "failed"
    assert "99-SP99" in rows[0].note


@responses.activate
def test_single_incidents_teradata_uses_base_version_in_url():
    """Upstream workaround: 12-SP3-TERADATA queries openQA as 12-SP3."""
    _register_job_groups(
        _job_group(106, "SLE 12 SP3 TERADATA Core Incidents"),
    )
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[],
        status=200,
    )

    rows = oqa_search.single_incidents(":12358:bash", ["12-SP3-TERADATA"], OPENQA)

    assert rows[0].version == "12-SP3-TERADATA"
    # URL uses the *base* version, not the TERADATA-suffixed one
    assert "version=12-SP3&" in rows[0].url
    assert "TERADATA" not in rows[0].url


# --- aggregated_updates ---


@responses.activate
def test_aggregated_updates_skips_excluded_versions():
    """TERADATA and 16.0 versions are dropped before any HTTP hit."""
    _register_job_groups(_job_group(367, "Core Maintenance Updates 15-SP5"))

    out = oqa_search.aggregated_updates(
        12358, ["15-SP4-TERADATA", "16.0"], 5, ["core"], OPENQA
    )
    assert out == []


@responses.activate
def test_aggregated_updates_finds_matching_build():
    """Walks back days until it finds an aggregated build covering the incident."""
    _register_job_groups(_job_group(367, "Core Maintenance Updates 15-SP5"))

    today = datetime.now().strftime("%Y%m%d") + "-1"

    # First call for today's build returns one job; its issues include 12358 -> match.
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/overview",
        json=[{"id": 999}],
        status=200,
    )
    # Job-issues lookup for job 999.
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs/999",
        json={"job": {"settings": {"INCIDENT_TEST_ISSUES": "12358,12359"}}},
        status=200,
    )
    # _query_version_status then makes running + failed queries.
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

    out = oqa_search.aggregated_updates(12358, ["15-SP5"], 5, ["core"], OPENQA)

    assert len(out) == 1
    group = out[0]
    assert group.group == "core"
    assert len(group.versions) == 1
    assert group.versions[0].version == "15-SP5"
    assert group.versions[0].status == "passed"
    assert today in group.versions[0].url


@responses.activate
def test_aggregated_updates_missing_after_window():
    """No matching builds across the whole window -> MISSING row with note."""
    _register_job_groups(_job_group(367, "Core Maintenance Updates 15-SP5"))

    # Every per-day query returns empty -> we exhaust the loop.
    for _ in range(3):
        responses.add(
            responses.GET,
            f"{OPENQA}/api/v1/jobs/overview",
            json=[],
            status=200,
        )

    out = oqa_search.aggregated_updates(12358, ["15-SP5"], 3, ["core"], OPENQA)
    row = out[0].versions[0]
    assert row.status == "missing"
    assert "in the last 3 days" in row.note


# --- build_checks ---

_HTML_INDEX = """
<html><body>
<a href="bash.SUSE_SLE-15-SP5_Update.x86_64.log">log1</a>
<a href="bash.SUSE_SLE-15-SP5_Update.aarch64.log">log2</a>
<a href="other-package.log">unrelated</a>
<a href="README.txt">no-log</a>
</body></html>
"""

_LOG_SHORT = """
[   12s] === 5 tests passed ===
[   13s] some other line
[   14s] 100% tests passed
"""

_LOG_LONG = "\n".join(
    [
        "[   12s] === run start ===",
        "[   13s] 5 tests passed",
        "[   14s] 6 tests passed",
        "[   15s] 7 tests passed",
        "[   16s] 8 tests passed",
        "[   17s] 9 tests passed",
        "[   18s] === run end ===",
    ]
)


@responses.activate
def test_build_checks_filters_logs_by_package_and_parses():
    """Index is filtered to package logs; matches extracted; short ones unfolded."""
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks",
        body=_HTML_INDEX,
        status=200,
        content_type="text/html",
    )
    # Register both .log entries the parser will extract from the index.
    for arch in ("x86_64", "aarch64"):
        responses.add(
            responses.GET,
            url=(
                f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks/"
                f"bash.SUSE_SLE-15-SP5_Update.{arch}.log"
            ),
            body=_LOG_SHORT,
            status=200,
        )
    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ":12358:bash", QAM, None
    )

    # Two .log files in the index match the package "bash"
    assert len(out) == 2
    # Short result list (≤ 4) -> no summary fold.
    assert all(entry.summary == "" for entry in out)
    assert all(entry.matches for entry in out)


@responses.activate
def test_build_checks_folds_long_match_lists():
    """When >4 matches, build_checks keeps first/last and stores a summary."""
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks",
        body='<a href="bash.x86_64.log">x</a>',
        status=200,
        content_type="text/html",
    )
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks/bash.x86_64.log",
        body=_LOG_LONG,
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ":12358:bash", QAM, None
    )
    assert len(out) == 1
    entry = out[0]
    assert entry.summary  # non-empty
    # Only first and last preserved when folded.
    assert len(entry.matches) == 2


@responses.activate
def test_build_checks_index_404_returns_empty():
    """A missing build_checks index is not an error -- just no entries."""
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks",
        status=404,
    )
    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ":12358:bash", QAM, None
    )
    assert out == []


def test_extract_test_results_custom_pattern_overrides_heuristics():
    """A user-supplied regex bypasses the heuristic blocklist."""
    log = "the syntax of make matters\nfoo: 3 widgets\nbar: 7 widgets"
    out = oqa_search.extract_test_results(log, r"\d+ widgets")
    assert out == ["foo: 3 widgets", "bar: 7 widgets"]


def test_extract_test_results_bad_regex_returns_empty():
    """Invalid regex logs a warning and returns []."""
    out = oqa_search.extract_test_results("anything", "[unclosed")
    assert out == []


def test_summarize_test_results_counts_passed_and_failed():
    lines = [
        "first line (ignored)",
        "5 passed",
        "3 failed, 2 passed",
        "last line (ignored)",
    ]
    summary = oqa_search.summarize_test_results(lines)
    # 7 passed (5 + 2), 3 failed; (len - 2) = 2 more results
    assert "2 more results" in summary
    assert "7 passed" in summary
    assert "3 failed" in summary


# --- get_incident_info ---


@responses.activate
def test_get_incident_info_returns_build_and_versions():
    responses.add(
        responses.GET,
        f"{DASHBOARD}/api/incident_settings/12358",
        json=[
            {
                "settings": {"BUILD": ":12358:bash", "DISTRI": "sle"},
                "version": "15-SP5",
                "flavor": "Server-DVD-Incidents",
            },
            {
                "settings": {"BUILD": ":12358:bash", "DISTRI": "sle"},
                "version": "15-SP4",
                "flavor": "Server-DVD-Incidents",
            },
            {
                "settings": {"BUILD": ":12358:bash", "DISTRI": "sle"},
                "version": "12-SP3",
                "flavor": "Server-TERADATA",
            },
        ],
        status=200,
    )

    build, versions = oqa_search.get_incident_info(DASHBOARD, 12358)
    assert build == ":12358:bash"
    assert versions is not None
    # TERADATA flavor causes the version to get the -TERADATA suffix
    assert "12-SP3-TERADATA" in versions
    assert "15-SP4" in versions
    assert "15-SP5" in versions


@responses.activate
def test_get_incident_info_no_builds_falls_back_to_package_name():
    """When /incident_settings is empty, fall back to /incidents/<id>."""
    responses.add(
        responses.GET,
        f"{DASHBOARD}/api/incident_settings/12358",
        json=[],
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
