//! The typed JSON report document (TeReGen `report.json`, schema v1.0,
//! `$id` `https://qam.suse.de/schema/report-template-v1.json`, served live at
//! `GET /api/v2/schema`).
//!
//! Pure data: no I/O, and not yet wired into the report lifecycle — retargeting
//! ingest from `MetadataEnvelope`/`TestReportBase` onto this type is future
//! work.
//!
//! # Rules every struct in this module follows
//!
//! - **Required** (schema `required`): a plain field, never `#[serde(default)]`
//!   nor `skip_serializing_if` — always emitted; missing on input is a parse
//!   error. A document the server has already validated never has one of its
//!   `required` keys absent, so tolerating a missing key here would mean
//!   silently constructing a report with no packages and no targets (P1-D1).
//! - **Optional** (may be genuinely absent): `Option<T>` +
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]` — an absent
//!   key stays absent on the round trip.
//! - **Required-nullable** (always present, value may be `null`): [`Req<T>`],
//!   never a plain `Option<T>`. serde's derive special-cases any field whose
//!   *declared type* is `Option<T>` (or, transitively, any type whose
//!   `Deserialize` impl calls `deserialize_option`) to silently default to
//!   `None` on a **missing** key — the exact "log + return `None`" failure
//!   class AGENTS.md forbids, and indistinguishable from a present `null`
//!   without this wrapper. Missing the key is a parse error, a present `null`
//!   deserializes to `Req(None)`, and it serializes back to `null` (never
//!   omitted). No field in this schema is both optional and nullable
//!   (verified field-by-field), so `Req<T>` is unambiguous everywhere it is
//!   used.
//! - Maps are `BTreeMap` for deterministic, server-matching key order:
//!   `issues`, `target.binaries`, `install_check.before`/`after`.
//! - Two objects — `testing` and `testing.openqa` — are schema-**open**
//!   (`additionalProperties: true`): their `extra: BTreeMap<String, Value>`
//!   catch-all preserves an unknown key across a round trip rather than
//!   silently dropping schema-legal server data (P1-D2). Every other object is
//!   schema-**closed**: plain serde already drops an unknown key there, and it
//!   is never re-emitted (re-emitting it would guarantee a `422`).

pub mod install;
pub mod issues;
pub mod review;
pub mod testing;
pub mod update;

use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_path_to_error::{Path, Segment};
use thiserror::Error;

pub use install::{Install, PlatformRef, Target, TestPlatform};
pub use issues::{Issue, IssueKey, IssueKeyParseError, IssueStatus, Issues, L3, Severity};
pub use review::{
    BuildLog, BuildLogResult, BuildResult, People, Review, ReviewSource, Reviewer, SlackRef,
    TesterEntry, Tristate,
};
pub use testing::{
    InstallCheck, Openqa, OpenqaFailure, OpenqaIncident, OpenqaInstall, OpenqaJob,
    OpenqaSummaryRow, OpenqaVerdict, Regression, TargetRef, Testing, TestingInstall,
};
pub use update::{Origin, Patch, Product, Update};

use crate::enums::RequestKind;
use crate::update_source::UpdateSource;

/// Wrapper for a **required-nullable** field: the JSON key must be present,
/// but its value may be `null`.
///
/// Plain `Option<T>` cannot express this — see the module doc's "Rules"
/// section. `Req<T>` never delegates to `Option<T>`'s own `Deserialize` impl
/// (which is what triggers serde's missing-key special case), so a missing
/// key is a parse error while a present `null` deserializes to `Req(None)`.
/// `Deref`s to `Option<T>` for ergonomic reads (`doc.verdict.is_some()`,
/// `*doc.verdict == Some(Verdict::Passed)`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Req<T>(pub Option<T>);

impl<T> Req<T> {
    /// Unwraps into the inner `Option<T>`.
    #[must_use]
    pub fn into_inner(self) -> Option<T> {
        self.0
    }
}

impl<T> From<Option<T>> for Req<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T> From<Req<T>> for Option<T> {
    fn from(value: Req<T>) -> Self {
        value.0
    }
}

impl<T> Deref for Req<T> {
    type Target = Option<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Serialize> Serialize for Req<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // `Option<T>`'s own (non-derive) `Serialize` impl always emits `null`
        // for `None` — exactly what a required-nullable field needs.
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Req<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Routing through `serde_json::Value` (rather than `Option::<T>::
        // deserialize`) is deliberate: `Option<T>`'s impl calls
        // `deserializer.deserialize_option`, which is exactly the hook serde's
        // generated missing-field handling special-cases to succeed with
        // `None` — reintroducing the hazard `Req` exists to close. Every
        // caller of this type already only ever parses JSON, so coupling this
        // helper to `serde_json::Value` costs nothing in practice.
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.is_null() {
            Ok(Self(None))
        } else {
            T::deserialize(value)
                .map(|v| Self(Some(v)))
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Root of the schema-v1.0 report document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportDocument {
    pub schema_version: SchemaVersion,
    /// Normalized review request id (schema pattern `^[A-Za-z0-9:._-]+$`).
    /// Kept as a plain string in this phase — nothing here parses it into an
    /// [`RequestReviewID`](crate::RequestReviewID); that is Phase 3's job.
    pub id: String,
    pub kind: DocumentKind,
    pub workflow: DocWorkflow,
    /// Time the pipeline produced this document, RFC 3339. Kept as a string:
    /// `chrono` is not a `mtui-types` dependency and nothing in this phase
    /// does date arithmetic.
    pub generated_at: String,
    /// Overall tester verdict from the log's `SUMMARY:` line, null until
    /// decided.
    pub verdict: Req<Verdict>,
    /// Report-level free text, from the `comment:` under `SUMMARY:`.
    pub comment: Req<String>,
    pub people: People,
    pub update: Update,
    pub install: Install,
    pub issues: Issues,
    pub testing: Testing,
    /// Absent for `kind: pi` (the schema's first `allOf` conditional requires
    /// it for `maintenance`/`slfo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
}

impl ReportDocument {
    /// `true` when this document already carries tester-authored content:
    /// a non-null `verdict`/`comment`, any entry in `people.testers`, a
    /// `testing.install` block, or a `testing.regression` with a non-null
    /// `verdict`/`comment`.
    ///
    /// `testing.openqa` is deliberately excluded: the pipeline pre-fills it in
    /// every freshly generated document, so its presence is not evidence of
    /// tester content — `regenerate`'s discard-guard consults this, and
    /// gating on `openqa` there would refuse every regenerate.
    #[must_use]
    pub fn has_tester_content(&self) -> bool {
        self.verdict.is_some()
            || self.comment.is_some()
            || !self.people.testers.is_empty()
            || self.testing.install.is_some()
            || self
                .testing
                .regression
                .as_ref()
                .is_some_and(|r| r.verdict.is_some() || r.comment.is_some())
    }
}

impl FromStr for ReportDocument {
    type Err = DocumentError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let mut de = serde_json::Deserializer::from_str(raw);
        serde_path_to_error::deserialize(&mut de).map_err(DocumentError::from)
    }
}

/// The schema version literal this crate understands. Only `"1.0"`
/// deserializes; the value always serializes back to `"1.0"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchemaVersion;

/// The one schema-version string [`SchemaVersion`] accepts and emits.
pub const SCHEMA_VERSION_STR: &str = "1.0";

impl Serialize for SchemaVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(SCHEMA_VERSION_STR)
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw == SCHEMA_VERSION_STR {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported schema_version {raw:?}, expected {SCHEMA_VERSION_STR:?}"
            )))
        }
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(SCHEMA_VERSION_STR)
    }
}

/// Update format (`kind`). Wire values are the exact lowercase schema tokens
/// `maintenance`/`slfo`/`pi` — distinct from [`RequestKind`]'s
/// `Maintenance`/`SLFO`/`PI` wire form (P1-D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentKind {
    Maintenance,
    Slfo,
    Pi,
}

impl From<DocumentKind> for RequestKind {
    fn from(kind: DocumentKind) -> Self {
        match kind {
            DocumentKind::Maintenance => Self::Maintenance,
            DocumentKind::Slfo => Self::Slfo,
            DocumentKind::Pi => Self::Pi,
        }
    }
}

impl From<RequestKind> for DocumentKind {
    fn from(kind: RequestKind) -> Self {
        match kind {
            RequestKind::Maintenance => Self::Maintenance,
            RequestKind::Slfo => Self::Slfo,
            RequestKind::Pi => Self::Pi,
        }
    }
}

/// Pipeline that produced the update (`workflow`), selecting the shape of
/// `update.origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocWorkflow {
    Obs,
    Gitea,
}

impl From<DocWorkflow> for UpdateSource {
    fn from(workflow: DocWorkflow) -> Self {
        match workflow {
            DocWorkflow::Obs => Self::Obs,
            DocWorkflow::Gitea => Self::Git,
        }
    }
}

/// Overall tester verdict (`verdict`, `testing.install.verdict`,
/// `testing.regression.verdict`, `install_check.verdict`): the
/// `PASSED`/`FAILED` vocabulary. Distinct from
/// [`OpenqaVerdict`]'s lowercase `passed`/`failed`
/// (P1-D4) — sharing one type across both wire vocabularies is how a `422`
/// gets shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    #[serde(rename = "PASSED")]
    Passed,
    #[serde(rename = "FAILED")]
    Failed,
}

/// Error produced when a raw JSON document fails to parse into a
/// [`ReportDocument`] (P1-D1: a missing required key is a typed error, never
/// a silent default). Carries the same RFC 6901 pointer vocabulary Phase 5
/// will see in a `422 {pointers: [...]}`.
///
/// A missing-*field* error (the field's key itself was never present) is only
/// tracked to the *object* that was missing it — the field name is not a
/// distinct path segment there, since deserialization never descended into
/// it. `source`'s message names the missing field in that case.
#[derive(Debug, Error)]
#[error("{pointer}: {source}")]
pub struct DocumentError {
    /// RFC 6901 pointer to the offending location, e.g.
    /// `/install/targets/0/arch`. `/` (root) when the error occurred before
    /// descending into any field.
    pub pointer: String,
    #[source]
    source: serde_json::Error,
}

impl From<serde_path_to_error::Error<serde_json::Error>> for DocumentError {
    fn from(err: serde_path_to_error::Error<serde_json::Error>) -> Self {
        let pointer = pointer_from_path(err.path());
        Self {
            pointer,
            source: err.into_inner(),
        }
    }
}

/// Render a `serde_path_to_error::Path` as an RFC 6901 JSON pointer
/// (`~`→`~0`, `/`→`~1` in map keys; a sequence segment renders as its plain
/// index).
fn pointer_from_path(path: &Path) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            Segment::Map { key } => {
                out.push('/');
                out.push_str(&key.replace('~', "~0").replace('/', "~1"));
            }
            Segment::Seq { index } => {
                out.push('/');
                out.push_str(&index.to_string());
            }
            Segment::Enum { variant } => {
                out.push('/');
                out.push_str(variant);
            }
            Segment::Unknown => {}
        }
    }
    if out.is_empty() { "/".to_owned() } else { out }
}

/// Every RFC 6901 pointer present in `original` but absent from
/// `round_tripped` (P1-D2's "unknown key dropped" signal). Object keys are
/// compared recursively by walking both trees in lockstep; a key whose value
/// merely *differs* (rather than being wholly absent) is not reported here —
/// that is a round-trip-correctness bug, the property [`ReportDocument`]'s own
/// semantic-round-trip test (P1-D3) already catches.
#[must_use]
pub fn dropped_pointers(original: &Value, round_tripped: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_dropped(original, round_tripped, &mut String::new(), &mut out);
    out
}

fn collect_dropped(
    original: &Value,
    round_tripped: &Value,
    path: &mut String,
    out: &mut Vec<String>,
) {
    match (original, round_tripped) {
        (Value::Object(orig_map), Value::Object(rt_map)) => {
            for (key, orig_val) in orig_map {
                let mark = path.len();
                path.push('/');
                path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                match rt_map.get(key) {
                    Some(rt_val) => collect_dropped(orig_val, rt_val, path, out),
                    None => out.push(path.clone()),
                }
                path.truncate(mark);
            }
        }
        (Value::Array(orig_arr), Value::Array(rt_arr)) => {
            for (index, orig_val) in orig_arr.iter().enumerate() {
                let mark = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                if let Some(rt_val) = rt_arr.get(index) {
                    collect_dropped(orig_val, rt_val, path, out);
                }
                path.truncate(mark);
            }
        }
        _ => {}
    }
}

/// Parses `raw` into a [`ReportDocument`], then logs (`tracing::warn!`, once
/// per pointer) every key [`dropped_pointers`] finds between the raw document
/// and the model's re-serialised form (P1-D2). The two schema-open objects
/// (`testing`, `testing.openqa`) never trigger this: their typed
/// `extra: BTreeMap<String, Value>` catch-all round-trips every key they see.
/// This is a diagnostic side effect, not a validation gate — the parsed
/// document is returned unchanged.
///
/// # Errors
///
/// Returns the same [`DocumentError`] [`ReportDocument::from_str`] would.
pub fn parse_and_warn_on_dropped_keys(raw: &str) -> Result<ReportDocument, DocumentError> {
    let doc: ReportDocument = raw.parse()?;
    // `raw` already parsed successfully above, so re-parsing it as a bare
    // `Value` cannot fail.
    let original: Value =
        serde_json::from_str(raw).expect("raw already parsed successfully as ReportDocument");
    let round_tripped = serde_json::to_value(&doc).expect("ReportDocument always serializes");
    for pointer in dropped_pointers(&original, &round_tripped) {
        tracing::warn!(pointer, "report document: unknown key dropped during parse");
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_rejects_unsupported_value() {
        let err = "\"1.1\"".parse::<serde_json::Value>().unwrap();
        let result: Result<SchemaVersion, _> = serde_json::from_value(err);
        assert!(result.is_err());
    }

    #[test]
    fn schema_version_accepts_and_round_trips_1_0() {
        let v: SchemaVersion = serde_json::from_str("\"1.0\"").unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"1.0\"");
        assert_eq!(v.to_string(), "1.0");
    }

    #[test]
    fn document_kind_wrong_case_is_rejected_lowercase_parses() {
        assert!(serde_json::from_str::<DocumentKind>("\"PI\"").is_err());
        let kind: DocumentKind = serde_json::from_str("\"pi\"").unwrap();
        assert_eq!(kind, DocumentKind::Pi);
    }

    #[test]
    fn verdict_null_round_trips_and_absent_is_an_error() {
        #[derive(Debug, Serialize, Deserialize)]
        struct Holder {
            verdict: Req<Verdict>,
        }
        let h: Holder = serde_json::from_str(r#"{"verdict": null}"#).unwrap();
        assert_eq!(h.verdict.into_inner(), None);
        assert_eq!(serde_json::to_string(&h).unwrap(), r#"{"verdict":null}"#);
        assert!(serde_json::from_str::<Holder>("{}").is_err());
    }

    #[test]
    fn req_rejects_missing_key_where_plain_option_would_not() {
        // Pin the exact hazard `Req<T>` exists to close: a plain `Option<T>`
        // field parses a missing key as `None` regardless of attributes.
        #[derive(Debug, Deserialize)]
        struct PlainOption {
            #[allow(dead_code)]
            verdict: Option<Verdict>,
        }
        assert!(serde_json::from_str::<PlainOption>("{}").is_ok());

        #[derive(Debug, Deserialize)]
        struct WithReq {
            #[allow(dead_code)]
            verdict: Req<Verdict>,
        }
        assert!(serde_json::from_str::<WithReq>("{}").is_err());
    }

    #[test]
    fn req_rejects_wrong_type_for_the_inner_value() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            #[allow(dead_code)]
            verdict: Req<Verdict>,
        }
        assert!(serde_json::from_str::<Holder>(r#"{"verdict": 42}"#).is_err());
    }

    #[test]
    fn req_derefs_to_option_and_converts_back() {
        let req = Req(Some(Verdict::Passed));
        // Deref: ergonomic reads without unwrapping the newtype.
        assert!(req.is_some());
        assert_eq!(*req, Some(Verdict::Passed));
        // The reverse `From` conversion.
        let back: Option<Verdict> = req.into();
        assert_eq!(back, Some(Verdict::Passed));
    }

    #[test]
    fn parse_and_warn_on_dropped_keys_returns_the_document_and_reports_drops() {
        let clean = r#"{
            "schema_version": "1.0", "id": "x", "kind": "pi",
            "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {"packager": "p", "source_packages": ["a"], "origin": {},
                       "products": [{"name": "n", "version": "v", "archs": ["x86_64"]}],
                       "patches": [{"id": "1", "title": "t"}]},
            "install": {"repository": "http://x/", "targets": [{
                "product": "n", "version": "v", "arch": "x86_64",
                "repository": "http://x/r", "binaries": {"a": "1-1.x86_64"}
            }], "test_platforms": []},
            "issues": {}, "testing": {}
        }"#;
        let doc = parse_and_warn_on_dropped_keys(clean).unwrap();
        assert_eq!(doc.id, "x");

        let with_stray = clean.replace(
            "\"issues\": {}",
            "\"issues\": {}, \"stray_top_level_key\": true",
        );
        // A stray key on the closed root object is dropped, not rejected —
        // the same call still returns the parsed document.
        let doc = parse_and_warn_on_dropped_keys(&with_stray).unwrap();
        assert_eq!(doc.id, "x");
    }

    #[test]
    fn doc_workflow_gitea_converts_to_update_source_git() {
        assert_eq!(UpdateSource::from(DocWorkflow::Gitea), UpdateSource::Git);
        assert_eq!(UpdateSource::from(DocWorkflow::Obs), UpdateSource::Obs);
    }

    #[test]
    fn document_kind_round_trips_through_request_kind() {
        for (kind, req) in [
            (DocumentKind::Maintenance, RequestKind::Maintenance),
            (DocumentKind::Slfo, RequestKind::Slfo),
            (DocumentKind::Pi, RequestKind::Pi),
        ] {
            assert_eq!(RequestKind::from(kind), req);
            assert_eq!(DocumentKind::from(req), kind);
        }
    }

    #[test]
    fn from_str_reports_pointer_for_a_missing_nested_required_field() {
        // `install.targets[0]` is missing its required `arch` key: the path
        // tracker descends into the map before the missing-field check fires,
        // so the pointer names the containing object.
        let raw = r#"{
            "schema_version": "1.0", "id": "x", "kind": "pi",
            "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {"packager": "p", "source_packages": ["a"], "origin": {},
                       "products": [{"name": "n", "version": "v", "archs": ["x86_64"]}]},
            "install": {"repository": "http://x/", "targets": [{
                "product": "n", "version": "v",
                "repository": "http://x/r", "binaries": {"a": "1-1.x86_64"}
            }], "test_platforms": []},
            "issues": {}, "testing": {}
        }"#;
        let err = raw.parse::<ReportDocument>().unwrap_err();
        assert_eq!(err.pointer, "/install/targets/0");
    }

    // --- has_tester_content ---

    const MAINTENANCE_OBS: &str =
        include_str!("../../tests/fixtures/document/maintenance_obs.json");
    const MAINTENANCE_ADDON: &str =
        include_str!("../../tests/fixtures/document/maintenance_addon.json");
    const SLFO_GITEA: &str = include_str!("../../tests/fixtures/document/slfo_gitea.json");
    const MAINTENANCE_OPENQA_L3: &str =
        include_str!("../../tests/fixtures/document/maintenance_openqa_l3.json");
    const PI: &str = include_str!("../../tests/fixtures/document/pi.json");

    /// Every freshly generated (server-produced) fixture: none of them carry
    /// tester content yet. `maximal.json` is deliberately excluded — it *is*
    /// tester-authored, by construction.
    #[test]
    fn has_tester_content_is_false_for_every_freshly_generated_fixture() {
        for (name, raw) in [
            ("maintenance_obs", MAINTENANCE_OBS),
            ("maintenance_addon", MAINTENANCE_ADDON),
            ("slfo_gitea", SLFO_GITEA),
            ("maintenance_openqa_l3", MAINTENANCE_OPENQA_L3),
            ("pi", PI),
        ] {
            let doc: ReportDocument = raw.parse().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                !doc.has_tester_content(),
                "{name} should carry no tester content"
            );
        }
    }

    /// A minimal, schema-valid document with no tester content anywhere.
    fn bare_document(id: &str) -> String {
        format!(
            r#"{{
                "schema_version": "1.0", "id": "{id}", "kind": "pi",
                "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
                "verdict": null, "comment": null,
                "people": {{"testers": [], "reviewer": {{"name": null}}}},
                "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                           "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}],
                           "patches": [{{"id": "1", "title": "t"}}]}},
                "install": {{"repository": "http://x/", "targets": [{{
                    "product": "n", "version": "v", "arch": "x86_64",
                    "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
                }}], "test_platforms": []}},
                "issues": {{}}, "testing": {{}}
            }}"#
        )
    }

    #[test]
    fn has_tester_content_false_for_bare_document() {
        let doc: ReportDocument = bare_document("x").parse().unwrap();
        assert!(!doc.has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_verdict() {
        let raw = bare_document("x").replace("\"verdict\": null", "\"verdict\": \"PASSED\"");
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_comment() {
        let raw = bare_document("x").replace("\"comment\": null", "\"comment\": \"note\"");
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_a_tester_entry() {
        let raw = bare_document("x").replace(
            "\"people\": {\"testers\": [], \"reviewer\": {\"name\": null}}",
            "\"people\": {\"testers\": [{\"name\": \"tester1\"}], \"reviewer\": {\"name\": null}}",
        );
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_testing_install() {
        let raw = bare_document("x").replace(
            "\"issues\": {}, \"testing\": {}",
            "\"issues\": {}, \"testing\": {\"install\": {\"verdict\": null, \"checks\": [], \
             \"comment\": null}}",
        );
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_regression_verdict() {
        let raw = bare_document("x").replace(
            "\"issues\": {}, \"testing\": {}",
            "\"issues\": {}, \"testing\": {\"regression\": {\"verdict\": \"PASSED\", \
             \"comment\": null}}",
        );
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    #[test]
    fn has_tester_content_true_for_regression_comment() {
        let raw = bare_document("x").replace(
            "\"issues\": {}, \"testing\": {}",
            "\"issues\": {}, \"testing\": {\"regression\": {\"verdict\": null, \
             \"comment\": \"note\"}}",
        );
        assert!(raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }

    /// `testing.openqa` alone is never evidence of tester content — the
    /// pipeline pre-fills it in every freshly generated document.
    #[test]
    fn has_tester_content_false_for_openqa_only() {
        let raw = bare_document("x").replace(
            "\"issues\": {}, \"testing\": {}",
            "\"issues\": {}, \"testing\": {\"openqa\": {}}",
        );
        assert!(!raw.parse::<ReportDocument>().unwrap().has_tester_content());
    }
}
