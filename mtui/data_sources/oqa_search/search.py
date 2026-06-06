"""Public entry points and supporting helpers for the openQA / QAM Dashboard search.

The three high-level entry points are :func:`single_incidents`,
:func:`aggregated_updates`, and :func:`build_checks`. Each returns a
list of typed result rows (defined in :mod:`.results`) that the command
layer renders. :func:`render_overview` produces the plain-text block
shared by the interactive command and the export injector.
"""

from __future__ import annotations

import re
from datetime import datetime, timedelta
from functools import lru_cache
from html.parser import HTMLParser
from logging import getLogger
from typing import Any, Final
from urllib.parse import quote, unquote

from typing_extensions import override

from .heuristics import (
    AGGREGATED_EXCLUDED_VERSIONS,
    AGGREGATED_GROUPS_TERMS,
    AGGREGATED_NAME_MAP,
    EXCLUDED_GROUPS,
    MICRO_TEMPLATE_IDENTIFIER,
    OQA_QUERY_STRINGS,
    SINGLE_INCIDENTS_TERMS,
    TESTSUITE_NUMBERS_PATTERN,
    TESTSUITE_SUMMARY_KEYWORDS,
    TESTSUITE_SUMMARY_PATTERNS,
    TESTSUITE_VISUAL_SEPARATORS,
    TESTSUITE_WORDS_BLOCKLIST,
)
from .http import _fetch_url_content, _get_json, _HTTPError
from .results import BuildCheckResult, GroupResult, VersionResult

logger = getLogger("mtui.connector.oqa_search")


# --- Incident info (Dashboard) ---


def get_incident_info(
    url_dashboard_qam: str, incident_id: int | str
) -> tuple[str, list[str] | None]:
    """Get the incident build name and the affected SLE versions.

    Returns ``(build, versions)``. ``versions`` is ``None`` when no
    openQA builds exist for the incident yet.

    Mirrors upstream ``_get_incident_info`` but kept as a public helper
    so the command layer can call it directly.
    """
    url = f"{url_dashboard_qam}/api/incident_settings/{incident_id}"
    incident_settings = _get_json(url)

    if not incident_settings:
        return _fallback_build(url_dashboard_qam, incident_id), None

    try:
        build = incident_settings[0]["settings"]["BUILD"]
    except (KeyError, IndexError):
        return _fallback_build(url_dashboard_qam, incident_id), None

    raw_versions = {
        (
            f"{i['version']}-TERADATA"
            if "TERADATA" in i.get("flavor", "")
            else i["version"]
        )
        for i in incident_settings
        if i.get("settings", {}).get("DISTRI") == "sle"
    }
    versions = sorted(raw_versions)

    return build, versions or None


def _fallback_build(url_dashboard_qam: str, incident_id: int | str) -> str:
    """Synthesize a build name from `/api/incidents/<id>` when no builds yet."""
    url = f"{url_dashboard_qam}/api/incidents/{incident_id}"
    incident_info = _get_json(url)
    packages = incident_info.get("packages") if isinstance(incident_info, dict) else []
    package = packages[0] if packages else ""
    return f":{incident_id}:{package}"


# --- openQA job-group enumeration ---


@lru_cache(maxsize=8)
def _fetch_openqa_groups(url_openqa: str) -> tuple[dict[str, Any], ...]:
    """Fetch and cache the full openQA job-group list for a given host.

    Cached as a tuple of dicts so the result is hashable and unchanging
    across calls within a process. Cache key is the openQA host so the
    function works correctly across multiple hosts in tests.
    """
    data = _get_json(f"{url_openqa}/api/v1/job_groups")
    return tuple(data) if isinstance(data, list) else ()


def _is_valid_template(group: dict[str, Any]) -> bool:
    template = group.get("template")
    return bool(template and MICRO_TEMPLATE_IDENTIFIER not in template)


def _is_name_matching(
    group: dict[str, Any], match_terms: list[str], excluded_terms: list[str]
) -> bool:
    name = group.get("name", "")
    return bool(
        any(term in name for term in match_terms)
        and not any(term in name for term in excluded_terms)
    )


def _extract_version(name: str) -> str:
    """Extract a normalized SLE version label from a job-group name."""
    # space-separated form: "12 SP5", optionally " TERADATA"
    m = re.search(r"(\d+)\s*SP\s*(\d+)(?:\s+TERADATA)?", name)
    if m:
        base = f"{m.group(1)}-SP{m.group(2)}"
        return f"{base}-TERADATA" if "TERADATA" in name else base
    # hyphen form: "12-SP3" or "12-SP3-TERADATA"
    m = re.search(r"(\d+)-SP\d+(?:-TERADATA)?", name)
    if m:
        return m.group(0)
    # dot form: "16.0"
    m = re.search(r"(\d+\.\d+)", name)
    if m:
        return m.group(0)
    return ""


def _extract_aggregated_name(name: str) -> str:
    for label, key in AGGREGATED_NAME_MAP.items():
        if label in name:
            return key
    return name.split()[0].lower() if name else ""


def _filter_openqa_groups(
    url_openqa: str,
    match_text: list[str],
    excluded_terms: list[str],
    name_extractor: Any,
) -> dict[str, int]:
    """Filter openQA job groups, keyed by the extractor's output."""
    return {
        name_extractor(group["name"]): group["id"]
        for group in _fetch_openqa_groups(url_openqa)
        if _is_name_matching(group, match_text, excluded_terms)
        and _is_valid_template(group)
    }


def get_incident_groups(url_openqa: str) -> dict[str, int]:
    """Single-Incidents / Core job groups keyed by SLE version."""
    return _filter_openqa_groups(
        url_openqa, SINGLE_INCIDENTS_TERMS, EXCLUDED_GROUPS, _extract_version
    )


def get_aggregated_groups(url_openqa: str) -> dict[str, int]:
    """Aggregated Updates job groups keyed by short name (core, sap, ...)."""
    return _filter_openqa_groups(
        url_openqa, AGGREGATED_GROUPS_TERMS, EXCLUDED_GROUPS, _extract_aggregated_name
    )


# --- openQA job lookups ---


def _get_group_id(url_openqa: str, key: str) -> int:
    """Resolve a SLE version or aggregated-group name to its openQA group id."""
    try:
        return get_incident_groups(url_openqa)[key]
    except KeyError:
        try:
            return get_aggregated_groups(url_openqa)[key]
        except KeyError as e:
            raise ValueError(
                f"Not a valid version (single incident) or "
                f"group (aggregated updates): {key}"
            ) from e


def _get_openqa_print_url(
    url_openqa: str, version: str, build: str, group_id: int
) -> str:
    """Browser-facing openQA overview URL."""
    return (
        f"{url_openqa}/tests/overview"
        f"?distri=sle&version={version}&build={build}&groupid={group_id}"
    )


def _get_openqa_build_url(
    state: str, url_openqa: str, version: str, build: str, group_id: int
) -> str:
    """API endpoint for filtered openQA jobs in a build."""
    if state not in OQA_QUERY_STRINGS:
        raise ValueError(f"Invalid openQA job state: {state}")

    base_url = (
        f"{url_openqa}/api/v1/jobs/overview"
        f"?distri=sle&version={version}&build={build}&groupid={group_id}"
    )
    return base_url + OQA_QUERY_STRINGS[state]


def _get_openqa_job_issues(url_openqa: str, job_id: int) -> set[int]:
    """Set of incident IDs being tested by an openQA job."""
    response = _get_json(f"{url_openqa}/api/v1/jobs/{job_id}")
    settings = response.get("job", {}).get("settings", {}) if response else {}

    issues: list[int] = []
    for k, v in settings.items():
        if "_TEST_ISSUES" in k.upper():
            issues.extend(int(i) for i in str(v).split(",") if i.strip().isdigit())
    return set(issues)


def _query_version_status(
    url_openqa: str, version: str, build: str, group_id: int
) -> VersionResult:
    """Resolve PASSED / FAILED / RUNNING for one openQA build."""
    # Upstream workaround: TERADATA jobs share the base version's URL.
    version_oqa = "12-SP3" if version == "12-SP3-TERADATA" else version

    print_url = _get_openqa_print_url(url_openqa, version_oqa, build, group_id)

    running_url = _get_openqa_build_url(
        "running", url_openqa, version_oqa, build, group_id
    )
    failed_url = _get_openqa_build_url(
        "failed", url_openqa, version_oqa, build, group_id
    )

    running_results = _get_json(running_url) or []
    failed_results = _get_json(failed_url) or []

    if failed_results:
        return VersionResult(
            version=version,
            url=print_url,
            status="failed",
            failed_count=len(failed_results),
        )
    if running_results:
        return VersionResult(
            version=version,
            url=print_url,
            status="running",
            running_count=len(running_results),
        )
    return VersionResult(version=version, url=print_url, status="passed")


# --- Build-check log parsing ---


class LogFileLinkParser(HTMLParser):
    """Collect ``.log`` link targets from an HTML directory index."""

    def __init__(self) -> None:
        super().__init__()
        self.log_files: list[str] = []

    @override
    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag != "a":
            return
        for attr, value in attrs:
            if attr == "href" and value and value.endswith(".log"):
                self.log_files.append(unquote(value))


def extract_test_results(log_text: str, custom_pattern: str | None = None) -> list[str]:
    """Extract test-summary lines from a build-check log.

    With ``custom_pattern`` the user's regex wins. Otherwise the same
    multi-stage heuristic upstream uses (numbers + blocklist + visual
    separators + summary keywords + canned patterns) decides.
    """
    matches: list[str] = []

    if custom_pattern:
        try:
            pattern = re.compile(custom_pattern, re.IGNORECASE)
        except re.error as e:
            logger.warning("Invalid regex pattern %r: %s", custom_pattern, e)
            return []
        matches.extend(line for line in log_text.splitlines() if pattern.search(line))
        return matches

    for line in log_text.splitlines():
        clean_line = re.sub(r"^\[\s*\d+s\]\s*", "", line)
        lower = clean_line.lower().strip()

        if not TESTSUITE_NUMBERS_PATTERN.search(lower):
            continue
        if any(blocked in lower for blocked in TESTSUITE_WORDS_BLOCKLIST):
            continue
        if any(sep in clean_line for sep in TESTSUITE_VISUAL_SEPARATORS):
            matches.append(line)
            continue
        if any(keyword in lower for keyword in TESTSUITE_SUMMARY_KEYWORDS):
            matches.append(line)
            continue
        if any(p.search(lower) for p in TESTSUITE_SUMMARY_PATTERNS):
            matches.append(line)

    return matches


def summarize_test_results(lines: list[str]) -> str:
    """Collapse a long match list into a one-line summary."""
    total_passed = 0
    total_failed = 0
    for line in lines[1:-1]:
        # N passed / N failed
        passed_match = re.search(r"(\d+) pass(?:ed)?", line, re.IGNORECASE)
        failed_match = re.search(r"(\d+) fail(?:ed)?", line, re.IGNORECASE)
        # PASS: N / FAIL: N
        passed_match = passed_match or re.search(
            r"#\s*pass(?:ed)?:?\s*(\d+)", line, re.IGNORECASE
        )
        failed_match = failed_match or re.search(
            r"#\s*fail(?:ed)?:?\s*(\d+)", line, re.IGNORECASE
        )
        if passed_match:
            total_passed += int(passed_match.group(1))
        if failed_match:
            total_failed += int(failed_match.group(1))
    return (
        f"({len(lines) - 2} more results, {total_passed} passed, {total_failed} failed)"
    )


# --- Public entry points used by the command layer ---


def single_incidents(
    build: str, versions: list[str], url_openqa: str
) -> list[VersionResult]:
    """Resolve openQA status for each SLE version of a single incident."""
    results: list[VersionResult] = []
    for version in versions:
        try:
            group_id = _get_group_id(url_openqa, version)
        except ValueError as e:
            logger.warning("%s", e)
            results.append(
                VersionResult(version=version, url="", status="failed", note=str(e))
            )
            continue
        try:
            results.append(_query_version_status(url_openqa, version, build, group_id))
        except _HTTPError as e:
            results.append(
                VersionResult(
                    version=version,
                    url="",
                    status="failed",
                    note=f"openQA query failed: {e}",
                )
            )
    return results


def aggregated_updates(
    incident_id: int | str,
    versions: list[str],
    days: int,
    aggregated_groups: list[str],
    url_openqa: str,
) -> list[GroupResult]:
    """Walk the last ``days`` days of aggregated builds for each group."""
    filtered_versions = [
        v
        for v in versions
        if not any(excl in v for excl in AGGREGATED_EXCLUDED_VERSIONS)
    ]

    if not filtered_versions:
        return []

    incident_id_int: int | None
    try:
        incident_id_int = int(incident_id)
    except (TypeError, ValueError):
        incident_id_int = None

    results: list[GroupResult] = []
    for group in aggregated_groups:
        try:
            group_id = _get_group_id(url_openqa, group)
        except ValueError as e:
            logger.warning("%s", e)
            continue

        group_result = GroupResult(group=group)
        for version in filtered_versions:
            group_result.versions.append(
                _scan_aggregated_for_version(
                    url_openqa,
                    version,
                    days,
                    group_id,
                    incident_id_int,
                )
            )
        results.append(group_result)
    return results


def _scan_aggregated_for_version(
    url_openqa: str,
    version: str,
    days: int,
    group_id: int,
    incident_id: int | None,
) -> VersionResult:
    """Find the most recent aggregated build covering the incident."""
    for i in range(days):
        build = f"{(datetime.now() - timedelta(days=i)).strftime('%Y%m%d')}-1"
        job_url = _get_openqa_build_url("all", url_openqa, version, build, group_id)
        try:
            jobs = _get_json(job_url) or []
        except _HTTPError as e:
            logger.debug("aggregated query for %s/%s failed: %s", version, build, e)
            continue
        if not jobs:
            continue

        job_id = jobs[0].get("id")
        if job_id is None:
            continue
        try:
            issues = _get_openqa_job_issues(url_openqa, int(job_id))
        except _HTTPError:
            continue

        if incident_id is None or incident_id in issues:
            try:
                return _query_version_status(url_openqa, version, build, group_id)
            except _HTTPError as e:
                return VersionResult(
                    version=version,
                    url="",
                    status="failed",
                    note=f"openQA query failed: {e}",
                )

    return VersionResult(
        version=version,
        url="",
        status="missing",
        note=f"No aggregated updates build for this incident in the last {days} days",
    )


def build_checks(
    product: str,
    incident_id: int | str,
    request_id: int,
    packages: list[str],
    url_qam: str,
    test_pattern: str | None = None,
) -> list[BuildCheckResult]:
    """Parse the qam.suse.de build_checks index and extract summaries.

    Args:
        product: The product kind (e.g., "Maintenance", "SLFO").
        incident_id: The incident/maintenance ID.
        request_id: The request/review ID.
        packages: List of package names to filter logs by.
        url_qam: Base URL for the QAM service.
        test_pattern: Optional regex pattern to extract test results.

    Returns:
        List of BuildCheckResult objects, one per matching .log file.

    """
    base_url = (
        f"{url_qam}/testreports/SUSE:{product}:{incident_id}:{request_id}/build_checks"
    )

    try:
        html_text = _fetch_url_content(base_url)
    except _HTTPError as e:
        logger.warning("build_checks index unavailable: %s", e)
        return []

    parser = LogFileLinkParser()
    parser.feed(html_text)

    # Filter .log files to those matching any package in this update
    logfiles = [log for log in parser.log_files if any(pkg in log for pkg in packages)]

    logger.debug(
        "Found %d .log files in build_checks index, %d match packages %r",
        len(parser.log_files),
        len(logfiles),
        packages,
    )
    if not logfiles:
        logger.warning("No build check logs found for packages %r", packages)
        return []

    out: list[BuildCheckResult] = []
    for log in logfiles:
        log_url = f"{base_url}/{quote(log)}"
        try:
            log_text = _fetch_url_content(log_url)
        except _HTTPError as e:
            logger.warning("build_check log %s unavailable: %s", log_url, e)
            out.append(BuildCheckResult(url=log_url))
            continue

        matches = extract_test_results(log_text, test_pattern)
        summary = ""
        if len(matches) > 4:
            summary = summarize_test_results(matches)
            matches = [matches[0], matches[-1]]
        out.append(BuildCheckResult(url=log_url, matches=matches, summary=summary))
    return out


# --- Plain-text renderer (shared by the command and the export injector) ---

# Begin/end markers wrap any block we insert into a testreport so the
# injector can find and replace it on re-export without disturbing
# adjacent user-typed content. Update with care -- these strings are a
# public contract with existing exported logs.
OVERVIEW_BEGIN_MARKER: Final[str] = "<!-- mtui openqa_overview begin -->"
OVERVIEW_END_MARKER: Final[str] = "<!-- mtui openqa_overview end -->"

# OBS prepends each build-log line with `[ <seconds>s]`; strip it when
# rendering for human consumption.
_OBS_TIMESTAMP_RE = re.compile(r"^\[\s*\d+s\]\s*")


def _strip_obs_timestamp(line: str) -> str:
    """Drop the OBS ``[  Ns]`` prefix from a build-log line."""
    return _OBS_TIMESTAMP_RE.sub("", line)


def render_overview(
    single_incidents_rows: list[VersionResult],
    aggregated_updates_rows: list[GroupResult],
    build_checks_rows: list[BuildCheckResult],
    *,
    skip_aggregated: bool = False,
) -> list[str]:
    """Render the overview as a list of plain-text lines (no ANSI).

    The output is markdown-ish: each section gets a `##` header so the
    block stays scannable when pasted into a testreport. No trailing
    newlines on individual entries; the caller joins them as needed.

    Used by both the interactive command (via the command's ``println``
    after stripping/no-op) and the export injector (which surrounds the
    block with begin/end markers).
    """
    lines: list[str] = []

    lines.append("## OpenQA Overview")
    lines.append("")

    # --- Single Incidents - Core ---
    if single_incidents_rows:
        lines.append("### Single Incidents - Core")
        lines.append("")
        for row in single_incidents_rows:
            lines.extend(_render_version_row(row))
        lines.append("")
    elif skip_aggregated or not aggregated_updates_rows:
        # Nothing to show in the visible sections -> upstream's "No
        # openQA builds" hint. When --no-aggregated is in effect we
        # cannot rely on the aggregated section to convey emptiness,
        # so the hint fires whenever single incidents is empty.
        lines.append("_No openQA builds for this incident yet._")
        lines.append("")

    # --- Aggregated Updates ---
    if not skip_aggregated:
        if aggregated_updates_rows:
            for group in aggregated_updates_rows:
                lines.append(f"### Aggregated Updates - {group.group.title()}")
                lines.append("")
                for row in group.versions:
                    lines.extend(_render_version_row(row))
                lines.append("")
        elif single_incidents_rows:
            # Single incidents found something, but aggregated produced
            # no groups (e.g. all versions excluded).
            lines.append("_No aggregated updates builds available for this incident._")
            lines.append("")

    # --- Build checks ---
    lines.append("### Build Checks")
    lines.append("")
    if build_checks_rows:
        for entry in build_checks_rows:
            lines.extend(_render_build_check(entry))
    else:
        lines.append("_No build checks for this incident._")
        lines.append("")

    return lines


def _render_version_row(row: VersionResult) -> list[str]:
    """Render one PASSED/FAILED/RUNNING/MISSING line as 1-2 plain lines."""
    out: list[str] = []
    if row.status == "missing":
        out.append(f"- {row.version}: {row.note}")
        return out

    head = f"- {row.version}"
    if row.url:
        head += f" -> {row.url}"
    out.append(head)

    if row.status == "failed":
        label = (
            f"  - FAILED ({row.failed_count} jobs)"
            if row.failed_count
            else "  - FAILED"
        )
        out.append(label)
    elif row.status == "running":
        label = (
            f"  - RUNNING/SCHEDULED ({row.running_count} jobs)"
            if row.running_count
            else "  - RUNNING/SCHEDULED"
        )
        out.append(label)
    else:
        out.append("  - PASSED")

    if row.note:
        out.append(f"  - note: {row.note}")
    return out


def _render_build_check(entry: BuildCheckResult) -> list[str]:
    """Render one build-check log entry."""
    out: list[str] = [f"- {entry.url}"]
    if not entry.matches:
        out.append(
            "  - No test results found (try using a custom pattern with --test-pattern)"
        )
        out.append("")
        return out

    if entry.summary:
        out.append(f"  - {_strip_obs_timestamp(entry.matches[0])}")
        out.append(f"  - {entry.summary}")
        out.append(f"  - {_strip_obs_timestamp(entry.matches[-1])}")
    else:
        out.extend(f"  - {_strip_obs_timestamp(line)}" for line in entry.matches)
    out.append("")
    return out
