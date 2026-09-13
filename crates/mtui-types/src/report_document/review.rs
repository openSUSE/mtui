//! `review` and `people` (`$defs/review`, `$defs/people`, `$defs/reviewer`,
//! `$defs/tester_entry` of the TeReGen report-document schema, v1.0).

use serde::{Deserialize, Serialize};

use super::Req;

/// Yes/no/null tristate (`$defs/tristate`) — required-nullable everywhere it
/// appears, so [`Req<bool>`] round-trips a present `null` and rejects an
/// absent key.
pub type Tristate = Req<bool>;

/// The three questions of `source code change review:` plus the per-package
/// build-log verdicts (`review`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub source: ReviewSource,
    pub build_log: BuildLog,
}

/// `review.source`: the three questions of `source code change review:`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewSource {
    pub new_version_or_package: Tristate,
    pub all_tracked_issues_documented: Tristate,
    pub untracked_changes: Tristate,
    pub comment: Req<String>,
}

/// `review.build_log`: the three `TEST_SUITE_*` questions and the per-package
/// verdicts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildLog {
    pub test_suite_present: Tristate,
    pub test_suite_sufficient: Tristate,
    pub test_suite_passed: Tristate,
    /// Assessed build logs, per package and architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<BuildLogResult>>,
    pub comment: Req<String>,
}

/// One `review.build_log.results[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildLogResult {
    pub package: String,
    /// Build repository, when the pipeline's package key carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub arch: String,
    pub result: BuildResult,
}

/// Per-package build-log verdict (`$defs/review.build_log.results[].result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildResult {
    Passed,
    Failed,
    Unknown,
    #[serde(rename = "no_tests")]
    NoTests,
}

/// Everyone who put their name to this report, by role (`people`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct People {
    /// Who ran mtui, one entry per export, append-only.
    pub testers: Vec<TesterEntry>,
    pub reviewer: Reviewer,
}

/// One `## export MTUI:` footer line, decomposed (`$defs/tester_entry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TesterEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtui: Option<String>,
    /// OS release mtui ran on, such as `openSUSE Tumbleweed-20260822`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// Server-stamped export time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// Who acked this report, and where (`$defs/reviewer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reviewer {
    /// Reviewer who acked the report, null until acked; superseded by
    /// `slack` when present.
    pub name: Req<String>,
    /// The Slack message the review was requested on, absent until one has
    /// been requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackRef>,
}

/// A Slack message reference (`reviewer.slack`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackRef {
    /// Canonical Slack channel ID, never a `#name`.
    pub channel: String,
    /// Slack message timestamp, which is also its ID within the channel.
    pub ts: String,
}
