"""Tests for ``mtui.data_sources.oqa_search``.

Covers the three public entry points (`single_incidents`,
`aggregated_updates`, `build_checks`) with `responses`-mocked HTTP.
The shared `lru_cache` on `_fetch_openqa_groups` is cleared between
tests so each test sees a clean slate.

Helper-level tests (group filters, heuristic match extraction) are
ported from the upstream oqa-search test suite to keep parity with the
reference implementation.
"""

from __future__ import annotations

import threading
from datetime import datetime
from pathlib import Path

import pytest
import responses

from mtui.data_sources import oqa_search

_FIXTURES = Path(__file__).parent / "fixtures" / "oqa_search"

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
    out = oqa_search.build_checks("Maintenance", 12358, 199773, ["bash"], QAM, None)

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

    out = oqa_search.build_checks("Maintenance", 12358, 199773, ["bash"], QAM, None)
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
    out = oqa_search.build_checks("Maintenance", 12358, 199773, ["bash"], QAM, None)
    assert out == []


@responses.activate
def test_build_checks_filters_multiple_packages():
    """Logs matching any package in the list are included."""
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks",
        body=_HTML_INDEX,
        status=200,
        content_type="text/html",
    )
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
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks/other-package.log",
        body=_LOG_SHORT,
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ["bash", "other-package"], QAM, None
    )

    assert len(out) == 3


@responses.activate
def test_build_checks_matches_flavored_python_package_to_source_log():
    """``pythonNNN-foo`` binary packages match their ``python-foo`` source log.

    Regression: the build_checks index names logs after the *source*
    package (``python-ecdsa``), but the update's package list contains the
    flavored binary names (``python313-ecdsa``). A plain substring check
    missed them; the normalized match recovers the log.
    """
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks",
        body='<a href="python-ecdsa.x86_64.log">x</a>',
        status=200,
        content_type="text/html",
    )
    responses.add(
        responses.GET,
        f"{QAM}/testreports/SUSE:Maintenance:12358:199773/build_checks/"
        "python-ecdsa.x86_64.log",
        body=_LOG_SHORT,
        status=200,
    )

    out = oqa_search.build_checks(
        "Maintenance", 12358, 199773, ["python313-ecdsa"], QAM, None
    )

    assert len(out) == 1
    assert out[0].url.endswith("python-ecdsa.x86_64.log")


@pytest.mark.parametrize(
    ("log", "packages", "expected"),
    [
        # Exact substring match (no flavor involved).
        ("bash.x86_64.log", ["bash"], True),
        # Flavored Python binary -> source-named log (2 and 3 digit flavors).
        ("python-ecdsa.x86_64.log", ["python313-ecdsa"], True),
        ("python-ecdsa.log", ["python38-ecdsa"], True),
        # Any package in the list may match.
        ("python-ecdsa.log", ["bash", "python311-ecdsa"], True),
        # Unrelated package does not match.
        ("python-ecdsa.log", ["python-rsa"], False),
        # `python3-foo` (single digit) is also normalized.
        ("python-foo.log", ["python3-foo"], True),
        # Regression: python3-tornado -> python-tornado (SUSE:Maintenance:44982:414912).
        ("python-tornado.x86_64.log", ["python3-tornado"], True),
        # Empty package list never matches.
        ("python-ecdsa.log", [], False),
    ],
)
def test_log_matches_package(log, packages, expected):
    """``log_matches_package`` normalizes flavored Python names before matching."""
    assert oqa_search.log_matches_package(log, packages) is expected


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


# --- Group-filter helpers (ported from upstream oqa-search) ---


@pytest.mark.parametrize(
    ("template", "expected"),
    [
        # SLE-Micro templates are filtered out via the MICRO_TEMPLATE_IDENTIFIER.
        ("sle-micro-2", False),
        # Missing / empty template => invalid.
        (None, False),
        ("", False),
        # Anything else is accepted.
        ("sle-15", True),
        ("sometext", True),
    ],
)
def test_is_valid_template(template, expected):
    group = {"id": 1, "name": "n", "template": template}
    assert oqa_search._is_valid_template(group) is expected


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        # Wrong product family.
        ("Maintenance: SLE 15 SP6 Core Incidents - DEV", False),
        ("Maintenance: Leap 15.6 Core Incidents", False),
        ("Maintenance: SLEM 5.4 Incidents", False),
        # Real single-incidents group => keep.
        ("Maintenance: SLE 12 SP5 Core Incidents", True),
    ],
)
def test_is_name_matching_single_incidents(name, expected):
    group = {"id": 1, "name": name, "template": "tpl"}
    assert (
        oqa_search._is_name_matching(
            group,
            list(oqa_search.SINGLE_INCIDENTS_TERMS),
            list(oqa_search.EXCLUDED_GROUPS),
        )
        is expected
    )


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        # Development / Micro / excluded buckets must not match aggregated.
        ("YaST Maintenance Updates - Development", False),
        ("Maintenance: SLE Micro / Public Cloud Maintenance Updates", False),
        ("Core Wicked Maintenance Updates", False),
        # Anything not in the match terms list is dropped too.
        ("Helm Chart required Images", False),
    ],
)
def test_is_name_matching_aggregated_updates(name, expected):
    group = {"id": 1, "name": name, "template": "tpl"}
    assert (
        oqa_search._is_name_matching(
            group,
            list(oqa_search.AGGREGATED_GROUPS_TERMS),
            list(oqa_search.EXCLUDED_GROUPS),
        )
        is expected
    )


@responses.activate
@pytest.mark.parametrize(
    ("match_text", "extractor", "bad_groups", "valid_groups", "expected"),
    [
        # Single incidents: SLE-Micro template + Wicked excluded term get
        # dropped; the two valid SP groups survive keyed by version.
        (
            list(oqa_search.SINGLE_INCIDENTS_TERMS),
            oqa_search._extract_version,
            [
                {
                    "id": 123,
                    "name": "Whatever Core Incidents",
                    "template": "sle-micro-testing",
                },
                {
                    "id": 321,
                    "name": "Wicked Core Incidents",
                    "template": "tpl",
                },
            ],
            [
                {
                    "id": 282,
                    "name": "Maintenance: SLE 12 SP5 Core Incidents",
                    "template": "tpl",
                },
                {
                    "id": 546,
                    "name": "Maintenance: SLE 15 SP6 Core Incidents",
                    "template": "tpl",
                },
            ],
            {"12-SP5": 282, "15-SP6": 546},
        ),
        # Aggregated: Development/Micro buckets get dropped; the two real
        # Maintenance Updates groups survive keyed by short name.
        (
            list(oqa_search.AGGREGATED_GROUPS_TERMS),
            oqa_search._extract_aggregated_name,
            [
                {
                    "id": 1,
                    "name": "YaST Maintenance Updates - Development",
                    "template": "tpl",
                },
                {
                    "id": 2,
                    "name": "Maintenance: SLE Micro / Public Cloud Maintenance Updates",
                    "template": "tpl",
                },
            ],
            [
                {
                    "id": 222,
                    "name": "Public Cloud Maintenance Updates",
                    "template": "tpl",
                },
                {
                    "id": 333,
                    "name": "Core Maintenance Updates",
                    "template": "tpl",
                },
            ],
            {"cloud": 222, "core": 333},
        ),
    ],
)
def test_filter_openqa_groups(
    match_text, extractor, bad_groups, valid_groups, expected
):
    """Verify that the bad groups are dropped and the survivors are keyed
    by the extractor's output. Mirrors upstream's parametrized test.
    """
    _register_job_groups(*bad_groups, *valid_groups)
    actual = oqa_search._filter_openqa_groups(
        OPENQA, match_text, list(oqa_search.EXCLUDED_GROUPS), extractor
    )
    assert actual == expected


# --- summarize_test_results (parametrized parity with upstream) ---


@pytest.mark.parametrize(
    ("lines", "expected_summary"),
    [
        # Two-line middle: "passed" + "failed" counted from distinct rows.
        (
            [
                "First line",
                "100 passed",
                "50 failed",
                "Last line",
            ],
            "(2 more results, 100 passed, 50 failed)",
        ),
        # Mixed pass/fail wording across multiple middle rows.
        (
            [
                "First line",
                "10 pass",
                "5 fail",
                "20 pass",
                "3 fail",
                "Last line",
            ],
            "(4 more results, 30 passed, 8 failed)",
        ),
        (
            [
                "[  949s] # TOTAL: 2901",
                "[  949s] # PASS:  2709",
                "[  949s] # SKIP:  151",
                "[  949s] # XFAIL: 0",
                "[  949s] # FAIL:  2",
                "[  949s] # XPASS: 0",
                "[  949s] # ERROR: 0",
                "[  949s] make[1]: Leaving directory '/usr/src/packages/BUILD/automake-1.16.5'",
            ],
            "(6 more results, 2709 passed, 2 failed)",
        ),
    ],
)
def test_summarize_test_results_parametrized(lines, expected_summary):
    assert oqa_search.summarize_test_results(lines) == expected_summary


# --- extract_test_results against real build-check logs (C1 scope) ---
#
# Two upstream packages are vendored under tests/fixtures/oqa_search/:
#
#   * iniparser -- minimal "OK (N tests)" summary line; exercises the
#     "summary keywords" branch of the heuristic.
#   * rust      -- multi-arch logs whose "test result: ok. N passed; N
#     failed" lines exercise the "summary patterns" branch and produce
#     enough matches to be folded by summarize_test_results.
#
# Each .log has a sibling .matches file with the exact lines upstream
# expects extract_test_results to return for that log. Keeping the
# fixture pairs in sync with upstream is the regression signal for the
# heuristic constants (TESTSUITE_*) that the connector copies verbatim.


def _fixture_pairs(package: str) -> list[tuple[Path, Path]]:
    """Pair each .log with its sibling .matches file (matched by arch)."""
    pkg_dir = _FIXTURES / package
    pairs: list[tuple[Path, Path]] = []
    for log in sorted(pkg_dir.glob("*.log")):
        # Filenames look like "...x86_64.log" -> "x86_64.matches".
        # Strip the extension and take the arch token after the last dot.
        arch = log.stem.rsplit(".", 1)[-1]
        matches_path = pkg_dir / f"{arch}.matches"
        if not matches_path.exists():
            pytest.fail(f"Missing matches fixture for {log}")
        pairs.append((log, matches_path))
    return pairs


@pytest.mark.parametrize("package", ["iniparser", "rust"])
def test_extract_test_results_real_logs(package):
    """For each (log, matches) fixture pair the heuristic output must
    match the upstream-curated expected lines exactly.
    """
    for log_path, matches_path in _fixture_pairs(package):
        log_text = log_path.read_text()
        # Upstream stores expected matches one-per-line; splitlines()
        # transparently handles both LF-terminated and bare last-line
        # cases (e.g. iniparser's single-line matches file).
        expected = matches_path.read_text().splitlines()
        assert oqa_search.extract_test_results(log_text) == expected, (
            f"heuristic drift for {package} / {log_path.name}"
        )


def test_extract_test_results_rust_folds_via_summarize():
    """End-to-end: the rust aarch64 log produces enough matches that
    summarize_test_results folds them into a "N more results" summary
    with non-zero pass/fail aggregates.
    """
    pkg_dir = _FIXTURES / "rust"
    log_text = (
        pkg_dir / "rust1.95.SUSE_SLE-15-SP3_Update:test.aarch64.log"
    ).read_text()
    matches = oqa_search.extract_test_results(log_text)
    assert len(matches) > 4, "expected the aarch64 fixture to produce >4 matches"
    summary = oqa_search.summarize_test_results(matches)
    assert "more results" in summary
    # The fixture rows include lines like "19848 passed; 0 failed", so
    # the totals must be > 0 for passed.
    assert "0 passed" not in summary
    assert "0 failed" in summary


# --- set_verify: TLS policy for the shared search session ---


@pytest.fixture
def oqa_http_module():
    """Yield the oqa_search http module, restoring its verify policy after."""
    from mtui.data_sources.oqa_search import http as _oqa_http

    original = _oqa_http._verify
    yield _oqa_http
    _oqa_http._verify = original
    _oqa_http._session.cache_clear()


def test_session_verifies_by_default(oqa_http_module):
    """The shared search session verifies TLS unless told otherwise."""
    _oqa_http = oqa_http_module
    _oqa_http.set_verify(True)
    assert _oqa_http._session().verify is True


def test_set_verify_rebuilds_session_with_new_policy(oqa_http_module):
    """Changing the policy clears the cache so the session is rebuilt."""
    _oqa_http = oqa_http_module

    _oqa_http.set_verify(True)
    first = _oqa_http._session()
    assert first.verify is True

    _oqa_http.set_verify(False)
    second = _oqa_http._session()
    # A new session object with the new policy (cache was cleared).
    assert second is not first
    assert second.verify is False

    # A CA-bundle path is honored verbatim.
    _oqa_http.set_verify("/etc/ssl/ca.pem")
    assert _oqa_http._session().verify == "/etc/ssl/ca.pem"


def test_set_verify_same_value_keeps_cached_session(oqa_http_module):
    """Setting the same policy does not rebuild the cached session."""
    _oqa_http = oqa_http_module
    _oqa_http.set_verify(True)
    first = _oqa_http._session()
    _oqa_http.set_verify(True)
    assert _oqa_http._session() is first


@responses.activate
def test_incident_jobs_drops_obsoleted_by_default():
    """obsoleted jobs are dropped unless include_obsoleted is set."""
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                {
                    "id": 1,
                    "test": "fips_smoke",
                    "result": "passed",
                    "settings": {"ARCH": "s390x"},
                },
                {
                    "id": 2,
                    "test": "ha_2nodes",
                    "result": "failed",
                    "settings": {"ARCH": "s390x"},
                },
                {
                    "id": 3,
                    "test": "old_run",
                    "result": "obsoleted",
                    "settings": {"ARCH": "x86_64"},
                },
            ]
        },
        status=200,
    )

    rows = oqa_search.incident_jobs(":git:5137:libica", OPENQA)

    assert [r.result for r in rows] == ["failed", "passed"]  # sorted, no obsoleted
    failed = next(r for r in rows if r.result == "failed")
    assert failed.test == "ha_2nodes"
    assert failed.arch == "s390x"
    assert failed.url == f"{OPENQA}/t2"


@responses.activate
def test_incident_jobs_include_obsoleted():
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                {
                    "id": 3,
                    "test": "x",
                    "result": "obsoleted",
                    "settings": {"ARCH": "x86_64"},
                }
            ]
        },
        status=200,
    )
    rows = oqa_search.incident_jobs(":b", OPENQA, include_obsoleted=True)
    assert len(rows) == 1
    assert rows[0].result == "obsoleted"


@responses.activate
def test_incident_jobs_captures_job_state():
    """The job ``state`` is kept so callers can tell pending from failed.

    A job that has not finished carries result ``none``; without its
    state (scheduled/running) the openqa_jobs command could not
    distinguish in-progress work from a genuine failure.
    """
    responses.add(
        responses.GET,
        f"{OPENQA}/api/v1/jobs",
        json={
            "jobs": [
                {
                    "id": 4,
                    "test": "mau_extratests",
                    "result": "none",
                    "state": "running",
                    "settings": {"ARCH": "x86_64"},
                },
                {
                    "id": 5,
                    "test": "fips_smoke",
                    "result": "passed",
                    "state": "done",
                    "settings": {"ARCH": "x86_64"},
                },
            ]
        },
        status=200,
    )

    rows = oqa_search.incident_jobs(":git:5137:libica", OPENQA)

    by_test = {r.test: r for r in rows}
    assert by_test["mau_extratests"].state == "running"
    assert by_test["mau_extratests"].result == "none"
    assert by_test["fips_smoke"].state == "done"


def test_incident_jobs_empty_build_makes_no_request():
    """A falsy build short-circuits with no HTTP call."""
    assert oqa_search.incident_jobs("", OPENQA) == []


# --- fan-out concurrency + order preservation ---
#
# These patch the per-item network functions with a Barrier-synchronised
# stub: the Barrier only trips once *every* item's worker is in flight, so
# a revert to a sequential loop makes the first worker block until the
# Barrier times out and the call raises -- i.e. the tests fail on a revert.
# Each also asserts the results keep input order.

_BARRIER_TIMEOUT = 10.0


def test_single_incidents_fans_out_and_preserves_order(monkeypatch):
    """single_incidents resolves versions concurrently, results stay in order."""
    versions = ["15-SP2", "15-SP3", "15-SP1", "12-SP5"]
    barrier = threading.Barrier(len(versions))

    monkeypatch.setattr(oqa_search.search, "_prewarm_openqa_groups", lambda url: None)
    monkeypatch.setattr(oqa_search.search, "_get_group_id", lambda url, key: 1)

    def _fake_status(url_openqa, version, build, group_id):
        barrier.wait(timeout=_BARRIER_TIMEOUT)
        return oqa_search.VersionResult(
            version=version, url=f"u/{version}", status="passed"
        )

    monkeypatch.setattr(oqa_search.search, "_query_version_status", _fake_status)

    rows = oqa_search.single_incidents("build", versions, OPENQA)

    assert [r.version for r in rows] == versions


def test_aggregated_updates_fans_out_and_preserves_order(monkeypatch):
    """Per-(group, version) day-walks run concurrently; nested order is kept."""
    groups = ["core", "sap"]
    versions = ["15-SP5", "15-SP4"]
    barrier = threading.Barrier(len(groups) * len(versions))

    monkeypatch.setattr(
        oqa_search.search,
        "_get_group_id",
        lambda url, key: {"core": 1, "sap": 2}[key],
    )

    def _fake_scan(url_openqa, version, days, group_id, incident_id):
        barrier.wait(timeout=_BARRIER_TIMEOUT)
        return oqa_search.VersionResult(
            version=version, url=f"g{group_id}/{version}", status="passed"
        )

    monkeypatch.setattr(oqa_search.search, "_scan_aggregated_for_version", _fake_scan)

    out = oqa_search.aggregated_updates(12358, versions, 5, groups, OPENQA)

    assert [g.group for g in out] == groups
    for group_result in out:
        assert [v.version for v in group_result.versions] == versions


def test_build_checks_fans_out_and_preserves_order(monkeypatch):
    """Per-log downloads run concurrently; rows stay in build-index order."""
    logs = ["bash.x86_64.log", "bash.aarch64.log", "bash.s390x.log"]
    index_html = "".join(f'<a href="{name}">{name}</a>' for name in logs)
    barrier = threading.Barrier(len(logs))

    def _fake_fetch(url):
        if url.endswith("/build_checks"):
            return index_html
        barrier.wait(timeout=_BARRIER_TIMEOUT)
        return f"content of {url}"

    monkeypatch.setattr(oqa_search.search, "_fetch_url_content", _fake_fetch)

    out = oqa_search.build_checks("Maintenance", 12358, 199773, ["bash"], QAM, None)

    assert [r.url.rsplit("/", 1)[-1] for r in out] == logs


def test_single_incidents_prewarms_job_groups_on_main_thread(monkeypatch):
    """single_incidents warms the group cache once, on the calling thread.

    Prewarming ``_fetch_openqa_groups`` before submitting the workers means
    the fanned-out ``_get_group_id`` calls hit the ``lru_cache`` instead of
    each issuing the large ``/api/v1/job_groups`` GET (which ``lru_cache``
    would not dedupe under concurrent misses). Spying the network call
    (``_get_json``, invoked only on a cache miss) and asserting the single
    fetch happens on the *main* thread makes this revert-sensitive: drop
    the prewarm and the lone fetch instead fires from a worker thread.
    """
    main_thread = threading.get_ident()
    fetch_threads: list[int] = []

    def _spy_get_json(url):
        fetch_threads.append(threading.get_ident())
        return [{"id": 490, "name": "SLE 15 SP5 Core Incidents", "template": "t"}]

    # _fetch_openqa_groups stays real (and lru_cached); only its network call
    # is spied, so worker cache hits do not record.
    monkeypatch.setattr(oqa_search.search, "_get_json", _spy_get_json)
    monkeypatch.setattr(
        oqa_search.search,
        "_query_version_status",
        lambda url, version, build, group_id: oqa_search.VersionResult(
            version=version, url="u", status="passed"
        ),
    )

    oqa_search.single_incidents("build", ["15-SP5", "15-SP4", "15-SP3"], OPENQA)

    assert fetch_threads == [main_thread]


def test_single_incidents_empty_versions_returns_empty_without_http(monkeypatch):
    """No versions -> empty result and not a single network call (no prewarm)."""
    calls: list[str] = []
    monkeypatch.setattr(oqa_search.search, "_get_json", lambda url: calls.append(url))

    assert oqa_search.single_incidents("build", [], OPENQA) == []
    assert calls == []


def test_single_incidents_isolates_a_failed_version(monkeypatch):
    """One version's _HTTPError yields a failed row; siblings still resolve.

    Pins the batch-isolation catch inside the fanned-out worker: a single
    query failure must not abort the whole overview, and order is kept.
    """
    versions = ["15-SP5", "15-SP4", "15-SP3"]
    monkeypatch.setattr(oqa_search.search, "_prewarm_openqa_groups", lambda url: None)
    monkeypatch.setattr(oqa_search.search, "_get_group_id", lambda url, key: 1)

    def _status(url_openqa, version, build, group_id):
        if version == "15-SP4":
            raise oqa_search._HTTPError("boom")
        return oqa_search.VersionResult(version=version, url="u", status="passed")

    monkeypatch.setattr(oqa_search.search, "_query_version_status", _status)

    rows = oqa_search.single_incidents("build", versions, OPENQA)

    assert [r.version for r in rows] == versions
    statuses = {r.version: r.status for r in rows}
    assert statuses == {"15-SP5": "passed", "15-SP4": "failed", "15-SP3": "passed"}
    failed = next(r for r in rows if r.version == "15-SP4")
    assert failed.note.startswith("openQA query failed")


@responses.activate
def test_aggregated_updates_unresolved_group_returns_empty():
    """Groups that don't resolve produce no GroupResult (prewarm still succeeds)."""
    # job_groups is reachable (so prewarm/_get_group_id do not raise _HTTPError)
    # but contains no matching group -> every _get_group_id raises ValueError.
    _register_job_groups(_job_group(490, "SLE 15 SP5 Core Incidents"))

    out = oqa_search.aggregated_updates(12358, ["15-SP5"], 5, ["nope"], OPENQA)
    assert out == []


def test_build_checks_isolates_an_unavailable_log(monkeypatch):
    """One unavailable log yields a placeholder row; siblings parse, order kept.

    Pins the batch-isolation catch inside the per-log worker.
    """
    logs = ["bash.x86_64.log", "bash.aarch64.log", "bash.s390x.log"]
    index_html = "".join(f'<a href="{name}">{name}</a>' for name in logs)

    def _fake_fetch(url):
        if url.endswith("/build_checks"):
            return index_html
        if url.endswith("bash.aarch64.log"):
            raise oqa_search._HTTPError("404")
        return f"PASSED {url}"

    monkeypatch.setattr(oqa_search.search, "_fetch_url_content", _fake_fetch)

    out = oqa_search.build_checks("Maintenance", 12358, 199773, ["bash"], QAM, None)

    assert [r.url.rsplit("/", 1)[-1] for r in out] == logs
    placeholder = out[1]
    assert placeholder.url.endswith("bash.aarch64.log")
    assert placeholder.matches == []
    assert placeholder.summary == ""
