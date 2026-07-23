//! Heuristics shared with upstream oqa-search.
//!
//! Ported verbatim from `mtui/data_sources/oqa_search/heuristics.py` to avoid
//! behavioural drift when comparing output against the upstream tool. These
//! constants drive job-group filtering and the build-check log line extraction;
//! the golden `.matches` fixtures are the regression signal that they stay in
//! sync with upstream.

use std::sync::LazyLock;

use regex::Regex;

/// Job-group templates containing this identifier are SLE-Micro and dropped.
pub(crate) const MICRO_TEMPLATE_IDENTIFIER: &str = "sle-micro";

/// Job-group name terms that exclude a group from either bucket.
pub(crate) const EXCLUDED_GROUPS: &[&str] =
    &["DEV", "Leap", "Development", "Micro", "Kernel", "Wicked"];

/// Job-group name terms that mark a Single-Incidents / Core group.
pub(crate) const SINGLE_INCIDENTS_TERMS: &[&str] = &["Core Incidents", "Core Staging"];

/// Job-group name terms that mark an Aggregated-Updates group.
pub(crate) const AGGREGATED_GROUPS_TERMS: &[&str] = &["Maintenance Updates"];

/// Aggregated-group name → short-name mapping (label, key).
pub(crate) const AGGREGATED_NAME_MAP: &[(&str, &str)] =
    &[("Public Cloud", "cloud"), ("SAP/HA", "sap")];

/// Versions excluded from aggregated-update scanning.
pub(crate) const AGGREGATED_EXCLUDED_VERSIONS: &[&str] = &["TERADATA", "16.0"];

/// The openQA job-state query-string suffix for `failed`.
const OQA_QUERY_FAILED: &str = "&result=failed&result=incomplete&result=timeout_exceeded";
/// The openQA job-state query-string suffix for `running`.
const OQA_QUERY_RUNNING: &str = "&state=scheduled&state=running";
/// The openQA job-state query-string suffix for `all`.
const OQA_QUERY_ALL: &str = "";

/// Resolve an openQA job-state name to its query-string suffix.
///
/// Returns `None` for an unknown state (upstream raised `ValueError`).
#[must_use]
pub(crate) fn oqa_query_string(state: &str) -> Option<&'static str> {
    match state {
        "failed" => Some(OQA_QUERY_FAILED),
        "running" => Some(OQA_QUERY_RUNNING),
        "all" => Some(OQA_QUERY_ALL),
        _ => None,
    }
}

/// A number surrounded by whitespace / parens / line ends, gating the
/// build-check line extractor (upstream `TESTSUITE_NUMBERS_PATTERN`).
pub(crate) static TESTSUITE_NUMBERS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s|\()\d+(?:$|\s|\))").expect("valid regex"));

/// Substrings that disqualify a build-check line (upstream
/// `TESTSUITE_WORDS_BLOCKLIST`). Matched case-insensitively against the
/// lower-cased line.
pub(crate) const TESTSUITE_WORDS_BLOCKLIST: &[&str] = &[
    "syntax", "--", "meson", "gcc", "clang", "make", "cmake", "/usr/bin", ".tap", ".sh", "t/",
    "todo", " - ", "duration", " + ", "group", "value", "doc", "stack", "errno", "tests in",
    "limit", "size", "test for", "creating", "task", "no tests", "thread", "server", "method",
    "object", "issue", "line", "set", "test_", "example", "flag", "print", "extra",
];

/// Visual separators that qualify a build-check line (upstream
/// `TESTSUITE_VISUAL_SEPARATORS`). Matched against the *cleaned* (not
/// lower-cased) line.
pub(crate) const TESTSUITE_VISUAL_SEPARATORS: &[&str] = &["===", "---"];

/// Summary keywords that qualify a build-check line (upstream
/// `TESTSUITE_SUMMARY_KEYWORDS`). Matched against the lower-cased line.
pub(crate) const TESTSUITE_SUMMARY_KEYWORDS: &[&str] = &[
    "result:",
    "summary",
    "out of",
    "tests passed",
    "tests failed",
];

/// Canned summary patterns that qualify a build-check line (upstream
/// `TESTSUITE_SUMMARY_PATTERNS`). Matched against the lower-cased line.
pub(crate) static TESTSUITE_SUMMARY_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\bok\s*\(",
        r"\d+%\s+tests?\s+passed",
        r"\d+\s+tests?\s+(ok|passed|failed|skipped)",
        r"#\s*(total|pass|fail|skip|xfail|xpass|error):",
        r"^(ok|fail|expected fail|unexpected pass|skipped):\s*\d+",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid regex"))
    .collect()
});

/// Normalises a flavored Python binary package name (`pythonNNN-foo`) to its
/// source form (`python-foo`), upstream `PYTHON_FLAVOR_RE` + `python-` sub.
pub(crate) static PYTHON_FLAVOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^python\d+-").expect("valid regex"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_string_resolves_known_states() {
        assert_eq!(oqa_query_string("failed"), Some(OQA_QUERY_FAILED));
        assert_eq!(oqa_query_string("running"), Some(OQA_QUERY_RUNNING));
        assert_eq!(oqa_query_string("all"), Some(""));
        assert_eq!(oqa_query_string("bogus"), None);
    }

    #[test]
    fn numbers_pattern_matches_bounded_number() {
        assert!(TESTSUITE_NUMBERS_PATTERN.is_match("OK (20 tests)"));
        assert!(TESTSUITE_NUMBERS_PATTERN.is_match("5 tests passed"));
        assert!(!TESTSUITE_NUMBERS_PATTERN.is_match("no numbers here"));
    }

    #[test]
    fn summary_patterns_compile_and_match() {
        assert!(
            TESTSUITE_SUMMARY_PATTERNS
                .iter()
                .any(|p| p.is_match("100% tests passed"))
        );
        assert!(
            TESTSUITE_SUMMARY_PATTERNS
                .iter()
                .any(|p| p.is_match("ok ("))
        );
    }

    #[test]
    fn python_flavor_normalizes() {
        assert_eq!(
            PYTHON_FLAVOR_RE.replace("python313-ecdsa", "python-"),
            "python-ecdsa"
        );
        assert_eq!(
            PYTHON_FLAVOR_RE.replace("python3-tornado", "python-"),
            "python-tornado"
        );
        // Non-flavored names are untouched.
        assert_eq!(PYTHON_FLAVOR_RE.replace("bash", "python-"), "bash");
    }
}
