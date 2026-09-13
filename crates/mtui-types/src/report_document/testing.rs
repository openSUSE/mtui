//! `testing` (schema-open), `openqa` (schema-open), install checks and
//! regression (`$defs/testing` and siblings of the TeReGen report-document
//! schema, v1.0).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Req, Verdict};

/// Grouped by the source of a result (`testing`).
///
/// Schema-**open** (`additionalProperties: true`, P1-D2): `extra` preserves
/// any key beyond the three named ones, since dropping it would discard
/// schema-legal server data on the next `PUT`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Testing {
    /// Everything openQA produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openqa: Option<Openqa>,
    /// Installation on real reference hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<TestingInstall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression: Option<Regression>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// `testing.openqa` — schema-**open** the same way as [`Testing`], with typed
/// `install`/`incident`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Openqa {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<OpenqaInstall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident: Option<OpenqaIncident>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// openQA installation jobs (`$defs/openqa_install`), the `Install tests:`
/// block at the foot of the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenqaInstall {
    /// Failed unless every job passed or softfailed, counting jobs that never
    /// ran. Lowercase `passed`/`failed` vocabulary — distinct from the tester
    /// [`Verdict`] (P1-D4).
    pub verdict: Req<OpenqaVerdict>,
    pub jobs: Vec<OpenqaJob>,
}

/// openQA's own verdict vocabulary (`passed`/`failed`), distinct from the
/// tester-facing [`Verdict`] (`PASSED`/`FAILED`) — see P1-D4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenqaVerdict {
    Passed,
    Failed,
}

/// One `openqa_install.jobs[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenqaJob {
    pub scenario: String,
    /// openQA result in its own lowercase vocabulary, such as `passed`,
    /// `softfailed` or `none` — deliberately a free string, not an enum:
    /// pinning one would reject a new openQA result value.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
}

/// Aggregate of all incident jobs (`$defs/openqa_incident`), with the
/// failures listed in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenqaIncident {
    pub summary: Vec<OpenqaSummaryRow>,
    pub failures: Vec<OpenqaFailure>,
}

/// One `openqa_incident.summary[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenqaSummaryRow {
    pub version: String,
    pub flavor: String,
    /// Schema `minItems: 1`.
    pub archs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<i64>,
    pub total: i64,
}

/// One `openqa_incident.failures[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenqaFailure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_modules: Option<Vec<String>>,
}

/// Installation on real reference hosts (`testing.install`), schema-closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestingInstall {
    /// Tester's install verdict from `INSTALL TESTS SUMMARY`, null until
    /// decided.
    pub verdict: Req<Verdict>,
    pub checks: Vec<InstallCheck>,
    /// `INSTALL TESTS SUMMARY` prose, with per-refhost comments folded in
    /// prefixed by host.
    pub comment: Req<String>,
}

/// One mtui reference-host block from `Test results by product-arch:`
/// (`$defs/install_check`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallCheck {
    /// Which `install.targets[]` row this check verifies, joined by exact
    /// product/version/arch match rather than a formatted string.
    pub target: TargetRef,
    pub refhost: String,
    pub verdict: Req<Verdict>,
    /// Versions installed before, keyed by package name; a missing package
    /// was not installed.
    pub before: BTreeMap<String, String>,
    /// Versions installed after, same convention.
    pub after: BTreeMap<String, String>,
}

/// The `product`/`version`/`arch` triple an [`InstallCheck`] joins against an
/// `install.targets[]` row by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRef {
    pub product: String,
    pub version: String,
    pub arch: String,
}

/// `testing.regression`, schema-closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regression {
    /// Tester's regression verdict from `REGRESSION TEST SUMMARY`, null until
    /// decided.
    pub verdict: Req<Verdict>,
    /// The `regression tests:` body, plus any prose from `REGRESSION TEST
    /// SUMMARY`.
    pub comment: Req<String>,
}
