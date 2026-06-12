"""openQA / QAM Dashboard / QAM build-check overview search.

Port of https://github.com/mjdonis/oqa-search adapted to mtui idioms:

* No printing -- functions return structured dataclasses; the
  ``openqa_overview`` command formats and prints.
* Logging via ``logger`` instead of stdout.
* Module-level ``lru_cache`` only fires inside the public entry points
  (never at import time).
* HTTP via a shared ``requests.Session`` with a (connect, read) timeout
  so a hung peer cannot block the REPL indefinitely.

The three high-level entry points are :func:`single_incidents`,
:func:`aggregated_updates`, and :func:`build_checks`. Each returns a
list of typed result rows that the command layer renders.

This package is split into four submodules along the upstream banner
seams so each piece is independently navigable:

* :mod:`.heuristics` -- the verbatim upstream constants/blocklists.
* :mod:`.results` -- the public dataclass return shapes.
* :mod:`.http` -- the shared session, ``_get_json``, ``_HTTPError``.
* :mod:`.search` -- everything else: dashboard / openQA / build-check
  helpers, the three public entry points, and the plain-text renderer.

The whitebox tests reach into private helpers (``_fetch_openqa_groups``,
``_filter_openqa_groups``, etc.); those names are re-exported here so
existing test patterns like ``oqa_search._fetch_openqa_groups.cache_clear()``
keep working.
"""

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
from .http import _fetch_url_content, _get_json, _HTTPError, _session, set_verify
from .results import BuildCheckResult, GroupResult, VersionResult
from .search import (
    OVERVIEW_BEGIN_MARKER,
    OVERVIEW_END_MARKER,
    LogFileLinkParser,
    _extract_aggregated_name,
    _extract_version,
    _fallback_build,
    _fetch_openqa_groups,
    _filter_openqa_groups,
    _get_group_id,
    _get_openqa_build_url,
    _get_openqa_job_issues,
    _get_openqa_print_url,
    _is_name_matching,
    _is_valid_template,
    _query_version_status,
    _render_build_check,
    _render_version_row,
    _scan_aggregated_for_version,
    _strip_obs_timestamp,
    aggregated_updates,
    build_checks,
    extract_test_results,
    get_aggregated_groups,
    get_incident_groups,
    get_incident_info,
    render_overview,
    single_incidents,
    summarize_test_results,
)

__all__ = [
    "AGGREGATED_EXCLUDED_VERSIONS",
    "AGGREGATED_GROUPS_TERMS",
    "AGGREGATED_NAME_MAP",
    "EXCLUDED_GROUPS",
    "MICRO_TEMPLATE_IDENTIFIER",
    "OQA_QUERY_STRINGS",
    "OVERVIEW_BEGIN_MARKER",
    "OVERVIEW_END_MARKER",
    "SINGLE_INCIDENTS_TERMS",
    "TESTSUITE_NUMBERS_PATTERN",
    "TESTSUITE_SUMMARY_KEYWORDS",
    "TESTSUITE_SUMMARY_PATTERNS",
    "TESTSUITE_VISUAL_SEPARATORS",
    "TESTSUITE_WORDS_BLOCKLIST",
    "BuildCheckResult",
    "GroupResult",
    "LogFileLinkParser",
    "VersionResult",
    "_HTTPError",
    "_extract_aggregated_name",
    "_extract_version",
    "_fallback_build",
    "_fetch_openqa_groups",
    "_fetch_url_content",
    "_filter_openqa_groups",
    "_get_group_id",
    "_get_json",
    "_get_openqa_build_url",
    "_get_openqa_job_issues",
    "_get_openqa_print_url",
    "_is_name_matching",
    "_is_valid_template",
    "_query_version_status",
    "_render_build_check",
    "_render_version_row",
    "_scan_aggregated_for_version",
    "_session",
    "_strip_obs_timestamp",
    "aggregated_updates",
    "build_checks",
    "extract_test_results",
    "get_aggregated_groups",
    "get_incident_groups",
    "get_incident_info",
    "render_overview",
    "set_verify",
    "single_incidents",
    "summarize_test_results",
]
