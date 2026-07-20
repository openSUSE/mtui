//! Plain-text renderer for the openQA overview and its begin/end markers.
//!
//! Ported from the renderer half of `mtui/data_sources/oqa_search/search.py`
//! (`render_overview`, `_render_version_row`, `_render_build_check`, the
//! `_strip_obs_timestamp` helper, and the `OVERVIEW_*` markers). The output is
//! markdown-ish plain text (no ANSI): each section gets a `##`/`###` header so
//! the block stays scannable when pasted into a testreport.
//!
//! Two consumers share this: the interactive `openqa_overview` command
//! (Phase 5) prints the lines directly, and the export injector
//! ([`crate::oqa_search`] via `mtui-testreport`) wraps the block with the
//! [`OVERVIEW_BEGIN_MARKER`]/[`OVERVIEW_END_MARKER`] so it can find and replace
//! its own block on re-export.

use std::sync::LazyLock;

use regex::Regex;

use super::results::{BuildCheckResult, GroupResult, VersionResult};

/// Begin marker wrapping an injected overview block.
///
/// **Public contract:** existing exported logs contain this exact string; the
/// injector relies on it to find and replace its own block. Change with care.
pub const OVERVIEW_BEGIN_MARKER: &str = "<!-- mtui openqa_overview begin -->";

/// End marker wrapping an injected overview block. See
/// [`OVERVIEW_BEGIN_MARKER`].
pub const OVERVIEW_END_MARKER: &str = "<!-- mtui openqa_overview end -->";

/// OBS prepends each build-log line with `[ <seconds>s]`; strip it when
/// rendering for human consumption (upstream `_OBS_TIMESTAMP_RE`).
static OBS_TIMESTAMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\s*\d+s\]\s*").expect("valid OBS timestamp regex"));

/// Drops the OBS `[  Ns]` prefix from a build-log line.
fn strip_obs_timestamp(line: &str) -> String {
    OBS_TIMESTAMP_RE.replace(line, "").into_owned()
}

/// Renders the overview as a list of plain-text lines (no ANSI, no trailing
/// newlines on individual entries — the caller joins them as needed).
///
/// Mirrors upstream `render_overview`. The `skip_aggregated` flag suppresses the
/// aggregated-updates section entirely (the user passed `--no-aggregated`).
#[must_use]
pub fn render_overview(
    single_incidents_rows: &[VersionResult],
    aggregated_updates_rows: &[GroupResult],
    build_checks_rows: &[BuildCheckResult],
    skip_aggregated: bool,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    lines.push("## OpenQA Overview".to_string());
    lines.push(String::new());

    // --- Single Incidents - Core ---
    if !single_incidents_rows.is_empty() {
        lines.push("### Single Incidents - Core".to_string());
        lines.push(String::new());
        for row in single_incidents_rows {
            lines.extend(render_version_row(row));
        }
        lines.push(String::new());
    } else if skip_aggregated || aggregated_updates_rows.is_empty() {
        // Nothing to show in the visible sections -> upstream's "No openQA
        // builds" hint. When --no-aggregated is in effect we cannot rely on the
        // aggregated section to convey emptiness, so the hint fires whenever
        // single incidents is empty.
        lines.push("_No openQA builds for this incident yet._".to_string());
        lines.push(String::new());
    }

    // --- Aggregated Updates ---
    if !skip_aggregated {
        if !aggregated_updates_rows.is_empty() {
            for group in aggregated_updates_rows {
                lines.push(format!(
                    "### Aggregated Updates - {}",
                    title_case(&group.group)
                ));
                lines.push(String::new());
                for row in &group.versions {
                    lines.extend(render_version_row(row));
                }
                lines.push(String::new());
            }
        } else if !single_incidents_rows.is_empty() {
            // Single incidents found something, but aggregated produced no
            // groups (e.g. all versions excluded).
            lines.push("_No aggregated updates builds available for this incident._".to_string());
            lines.push(String::new());
        }
    }

    // --- Build checks ---
    lines.push("### Build Checks".to_string());
    lines.push(String::new());
    if build_checks_rows.is_empty() {
        lines.push("_No build checks for this incident._".to_string());
        lines.push(String::new());
    } else {
        for entry in build_checks_rows {
            lines.extend(render_build_check(entry));
        }
    }

    lines
}

/// Renders one PASSED/FAILED/RUNNING/MISSING line as 1-2 plain lines
/// (upstream `_render_version_row`).
fn render_version_row(row: &VersionResult) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if row.status == "missing" {
        out.push(format!("- {}: {}", row.version, row.note));
        return out;
    }

    let mut head = format!("- {}", row.version);
    if !row.url.is_empty() {
        head.push_str(&format!(" -> {}", row.url));
    }
    out.push(head);

    match row.status.as_str() {
        "failed" => {
            let label = if row.failed_count != 0 {
                format!("  - FAILED ({} jobs)", row.failed_count)
            } else {
                "  - FAILED".to_string()
            };
            out.push(label);
        }
        "running" => {
            let label = if row.running_count != 0 {
                format!("  - RUNNING/SCHEDULED ({} jobs)", row.running_count)
            } else {
                "  - RUNNING/SCHEDULED".to_string()
            };
            out.push(label);
        }
        _ => out.push("  - PASSED".to_string()),
    }

    if !row.note.is_empty() {
        out.push(format!("  - note: {}", row.note));
    }
    out
}

/// Renders one build-check log entry (upstream `_render_build_check`).
fn render_build_check(entry: &BuildCheckResult) -> Vec<String> {
    let mut out: Vec<String> = vec![format!("- {}", entry.url)];
    if entry.matches.is_empty() {
        out.push(
            "  - No test results found (try using a custom pattern with --test-pattern)"
                .to_string(),
        );
        out.push(String::new());
        return out;
    }

    if !entry.summary.is_empty() {
        out.push(format!("  - {}", strip_obs_timestamp(&entry.matches[0])));
        out.push(format!("  - {}", entry.summary));
        out.push(format!(
            "  - {}",
            strip_obs_timestamp(entry.matches.last().expect("non-empty checked above"))
        ));
    } else {
        out.extend(
            entry
                .matches
                .iter()
                .map(|line| format!("  - {}", strip_obs_timestamp(line))),
        );
    }
    out.push(String::new());
    out
}

/// Capitalizes the first letter of each whitespace-separated word, matching
/// Python's `str.title()` for the simple group-name case (ASCII words).
fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_obs_timestamp_removes_prefix() {
        assert_eq!(
            strip_obs_timestamp("[   28s] All 9 tests passed"),
            "All 9 tests passed"
        );
        assert_eq!(strip_obs_timestamp("no prefix"), "no prefix");
    }

    #[test]
    fn title_case_matches_python_title() {
        assert_eq!(title_case("core"), "Core");
        assert_eq!(title_case("sap hana"), "Sap Hana");
    }

    #[test]
    fn empty_overview_shows_hints() {
        let lines = render_overview(&[], &[], &[], false);
        assert!(lines.contains(&"## OpenQA Overview".to_string()));
        assert!(lines.contains(&"_No openQA builds for this incident yet._".to_string()));
        assert!(lines.contains(&"### Build Checks".to_string()));
        assert!(lines.contains(&"_No build checks for this incident._".to_string()));
    }

    #[test]
    fn version_row_failed_with_count() {
        let row = VersionResult {
            version: "15-SP4".into(),
            url: "https://oqa/u2".into(),
            status: "failed".into(),
            failed_count: 3,
            ..Default::default()
        };
        let out = render_version_row(&row);
        assert_eq!(out[0], "- 15-SP4 -> https://oqa/u2");
        assert_eq!(out[1], "  - FAILED (3 jobs)");
    }

    #[test]
    fn version_row_missing_uses_note_only() {
        let row = VersionResult {
            version: "15-SP6".into(),
            status: "missing".into(),
            note: "no build".into(),
            ..Default::default()
        };
        assert_eq!(
            render_version_row(&row),
            vec!["- 15-SP6: no build".to_string()]
        );
    }

    #[test]
    fn build_check_summary_folds_first_and_last() {
        let entry = BuildCheckResult {
            url: "https://qam/xz.log".into(),
            matches: vec![
                "[  1s] first".into(),
                "[  2s] mid".into(),
                "[  3s] last".into(),
            ],
            summary: "3 passed".into(),
        };
        let out = render_build_check(&entry);
        assert_eq!(out[0], "- https://qam/xz.log");
        assert_eq!(out[1], "  - first");
        assert_eq!(out[2], "  - 3 passed");
        assert_eq!(out[3], "  - last");
        assert_eq!(out[4], "");
    }
}
