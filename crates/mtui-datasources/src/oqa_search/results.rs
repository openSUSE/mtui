//! Result types — the public return shapes of the entry points.
//!
//! Ported from `mtui/data_sources/oqa_search/results.py`. These are the typed
//! rows the command layer renders; the search functions never print.

/// One row in a Single Incidents / Aggregated Updates section.
///
/// `status` is one of: `"passed"`, `"failed"`, `"running"`, `"missing"` (no
/// openQA build found in the date window for aggregated updates).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VersionResult {
    /// The SLE version label (e.g. `15-SP5`).
    pub version: String,
    /// The browser-facing openQA overview URL (empty when unresolved).
    pub url: String,
    /// One of `passed` / `failed` / `running` / `missing`.
    pub status: String,
    /// Number of failed jobs (populated when `status == "failed"`).
    pub failed_count: usize,
    /// Number of running/scheduled jobs (populated when `status == "running"`).
    pub running_count: usize,
    /// A free-form note (e.g. the reason for a `missing`/`failed` row).
    pub note: String,
}

/// Aggregated Updates results for one job group (e.g. `core`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupResult {
    /// The short group name (e.g. `core`, `sap`, `cloud`).
    pub group: String,
    /// The per-version rows for this group.
    pub versions: Vec<VersionResult>,
}

/// One build-check log entry parsed from qam.suse.de.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildCheckResult {
    /// The full URL of the `.log` file.
    pub url: String,
    /// The extracted test-summary lines (folded to first/last when long).
    pub matches: Vec<String>,
    /// A one-line summary when the match list was folded (else empty).
    pub summary: String,
}

/// One openQA job for an incident build.
///
/// `result` is openQA's job result: `passed`, `softfailed`, `failed`,
/// `parallel_failed`, `incomplete`, `skipped` or `obsoleted` (superseded by a
/// retrigger). `test` is the scenario name (the meaningful field for judging
/// relevance — unlike the full job name it does not embed the build string).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobResult {
    /// The openQA job id.
    pub job_id: i64,
    /// The test/scenario name.
    pub test: String,
    /// The job architecture.
    pub arch: String,
    /// The openQA job result.
    pub result: String,
    /// The job group name (may be empty).
    pub group: String,
    /// The browser-facing job URL (`.../t<id>`).
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_result_defaults() {
        let r = VersionResult {
            version: "15-SP5".into(),
            status: "passed".into(),
            ..Default::default()
        };
        assert_eq!(r.failed_count, 0);
        assert_eq!(r.running_count, 0);
        assert!(r.url.is_empty());
        assert!(r.note.is_empty());
    }

    #[test]
    fn group_and_build_check_defaults() {
        let g = GroupResult {
            group: "core".into(),
            ..Default::default()
        };
        assert!(g.versions.is_empty());
        let b = BuildCheckResult {
            url: "u".into(),
            ..Default::default()
        };
        assert!(b.matches.is_empty());
        assert!(b.summary.is_empty());
    }
}
