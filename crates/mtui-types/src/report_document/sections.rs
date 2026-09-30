//! Sections of a [`ReportDocument`]: the named top-level slices a tester reads
//! and edits, the completeness walk over them, and validated writes.
//!
//! A write never mutates in place: it splices the new value into a serialised
//! copy, re-parses the whole candidate, and only then hands back the new
//! document, so a rejected write leaves the caller's document untouched.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use super::{DocumentError, IssueKey, IssueKeyParseError, ReportDocument, dropped_pointers};

/// A top-level section of the report document.
///
/// Not every top-level key is a section: `schema_version`, `id`, `kind`,
/// `workflow` and `generated_at` are identity and provenance, not content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Verdict,
    Comment,
    People,
    Update,
    Install,
    Issues,
    Testing,
    Review,
}

impl Section {
    /// Every section, in the server's listing order.
    pub const ALL: [Self; 8] = [
        Self::Verdict,
        Self::Comment,
        Self::People,
        Self::Update,
        Self::Install,
        Self::Issues,
        Self::Testing,
        Self::Review,
    ];

    /// The section's document key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verdict => "verdict",
            Self::Comment => "comment",
            Self::People => "people",
            Self::Update => "update",
            Self::Install => "install",
            Self::Issues => "issues",
            Self::Testing => "testing",
            Self::Review => "review",
        }
    }

    /// Whether a tester may write this section. `update` and `install` are
    /// owned by the pipeline and feed derived report state, so they are
    /// read-only here.
    #[must_use]
    pub const fn is_tester_writable(self) -> bool {
        !matches!(self, Self::Update | Self::Install)
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string names no [`Section`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown section {raw:?}; valid sections: {}", Section::ALL.map(Section::as_str).join(", "))]
pub struct UnknownSectionError {
    raw: String,
}

impl FromStr for Section {
    type Err = UnknownSectionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|section| section.as_str() == s)
            .ok_or_else(|| UnknownSectionError { raw: s.to_owned() })
    }
}

/// One row of [`ReportDocument::section_summaries`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionSummary {
    pub name: &'static str,
    /// Byte length of the section's compact JSON. Advisory: another
    /// implementation may count escapes differently.
    pub size: usize,
    /// Whether the section still holds a `null` leaf.
    pub has_unanswered: bool,
}

/// Result of [`ReportDocument::completeness`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completeness {
    pub complete: bool,
    /// RFC 6901 pointer of every `null` leaf in the document.
    pub unfilled: Vec<String>,
}

/// Why a section or issue write was refused. The document is unchanged in
/// every case.
#[derive(Debug, Error)]
pub enum SectionWriteError {
    #[error("section {0} is pipeline-owned and read-only")]
    ReadOnly(Section),
    #[error("section {0} is not present in this report")]
    Absent(Section),
    #[error("value does not fit the report schema: {0}")]
    Invalid(#[source] DocumentError),
    #[error("value carries keys the schema does not allow: {}", .0.join(", "))]
    Dropped(Vec<String>),
    #[error(
        "issues write must keep the same keys (added: [{}], removed: [{}]); \
         use report_issue_write to change one entry",
        .added.join(", "),
        .removed.join(", ")
    )]
    IssueKeysChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    #[error("no issue {0:?} in this report")]
    UnknownIssue(String),
    #[error(transparent)]
    BadIssueKey(#[from] IssueKeyParseError),
}

/// RFC 6901 pointer of every `null` leaf under `value`, each prefixed with
/// `prefix`. A root `null` yields `prefix` itself; object keys are visited in
/// sorted order and escaped (`~`→`~0`, `/`→`~1`).
#[must_use]
pub fn null_pointers(value: &Value, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_nulls(value, &mut prefix.to_owned(), &mut out);
    out
}

fn collect_nulls(value: &Value, path: &mut String, out: &mut Vec<String>) {
    match value {
        Value::Null => out.push(path.clone()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let mark = path.len();
                path.push('/');
                path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                collect_nulls(&map[key], path, out);
                path.truncate(mark);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let mark = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                collect_nulls(item, path, out);
                path.truncate(mark);
            }
        }
        _ => {}
    }
}

impl ReportDocument {
    fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("ReportDocument always serializes")
    }

    /// The JSON of one section, or `None` when this document lacks it
    /// (`review` on a `pi` report).
    #[must_use]
    pub fn section(&self, section: Section) -> Option<Value> {
        match self.to_value() {
            Value::Object(mut map) => map.remove(section.as_str()),
            _ => None,
        }
    }

    /// One summary per section this document has, in [`Section::ALL`] order.
    #[must_use]
    pub fn section_summaries(&self) -> Vec<SectionSummary> {
        let Value::Object(map) = self.to_value() else {
            return Vec::new();
        };
        Section::ALL
            .into_iter()
            .filter_map(|section| {
                let value = map.get(section.as_str())?;
                Some(SectionSummary {
                    name: section.as_str(),
                    size: serde_json::to_vec(value).map_or(0, |bytes| bytes.len()),
                    has_unanswered: !null_pointers(value, "").is_empty(),
                })
            })
            .collect()
    }

    /// Every `null` leaf across the whole document.
    #[must_use]
    pub fn completeness(&self) -> Completeness {
        let unfilled = null_pointers(&self.to_value(), "");
        Completeness {
            complete: unfilled.is_empty(),
            unfilled,
        }
    }

    /// A copy of this document with `section` replaced by `value`.
    ///
    /// # Errors
    ///
    /// Refused when the section is read-only or absent, when an `issues`
    /// write adds or drops a key, when the candidate no longer parses as a
    /// [`ReportDocument`], or when it carries keys the schema drops.
    pub fn with_section(&self, section: Section, value: Value) -> Result<Self, SectionWriteError> {
        if !section.is_tester_writable() {
            return Err(SectionWriteError::ReadOnly(section));
        }
        let mut candidate = self.to_value();
        let Some(map) = candidate.as_object_mut() else {
            return Err(SectionWriteError::Absent(section));
        };
        let Some(current) = map.get(section.as_str()) else {
            return Err(SectionWriteError::Absent(section));
        };
        if section == Section::Issues {
            check_issue_keys(current, &value)?;
        }
        map.insert(section.as_str().to_owned(), value);
        validated(candidate)
    }

    /// A copy of this document with the existing issue `issue_id` replaced by
    /// `value`.
    ///
    /// # Errors
    ///
    /// Refused when `issue_id` is malformed or names no issue in this
    /// document, or for the same validation reasons as
    /// [`with_section`](Self::with_section).
    pub fn with_issue(&self, issue_id: &str, value: Value) -> Result<Self, SectionWriteError> {
        let key: IssueKey = issue_id.parse()?;
        if !self.issues.contains_key(&key) {
            return Err(SectionWriteError::UnknownIssue(issue_id.to_owned()));
        }
        let mut candidate = self.to_value();
        if let Some(issues) = candidate.get_mut("issues").and_then(Value::as_object_mut) {
            issues.insert(key.as_str().to_owned(), value);
        }
        validated(candidate)
    }
}

fn check_issue_keys(current: &Value, proposed: &Value) -> Result<(), SectionWriteError> {
    let (Some(current), Some(proposed)) = (current.as_object(), proposed.as_object()) else {
        return Ok(());
    };
    let old: BTreeSet<&String> = current.keys().collect();
    let new: BTreeSet<&String> = proposed.keys().collect();
    let added: Vec<String> = new.difference(&old).map(|k| (*k).clone()).collect();
    let removed: Vec<String> = old.difference(&new).map(|k| (*k).clone()).collect();
    if added.is_empty() && removed.is_empty() {
        Ok(())
    } else {
        Err(SectionWriteError::IssueKeysChanged { added, removed })
    }
}

fn validated(candidate: Value) -> Result<ReportDocument, SectionWriteError> {
    let document: ReportDocument = serde_path_to_error::deserialize(&candidate)
        .map_err(|e| SectionWriteError::Invalid(DocumentError::from(e)))?;
    let dropped = dropped_pointers(&candidate, &document.to_value());
    if dropped.is_empty() {
        Ok(document)
    } else {
        Err(SectionWriteError::Dropped(dropped))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const MAINTENANCE_OBS: &str =
        include_str!("../../tests/fixtures/document/maintenance_obs.json");
    const PI: &str = include_str!("../../tests/fixtures/document/pi.json");
    const MAXIMAL: &str = include_str!("../../tests/fixtures/document/maximal.json");

    fn doc(raw: &str) -> ReportDocument {
        raw.parse().expect("fixture parses")
    }

    fn section_of(doc: &ReportDocument, section: Section) -> Value {
        doc.section(section).expect("section present")
    }

    // --- Section ---

    #[test]
    fn section_names_round_trip_and_are_case_sensitive() {
        for section in Section::ALL {
            assert_eq!(section.as_str().parse::<Section>().unwrap(), section);
        }
        let err = "Verdict".parse::<Section>().unwrap_err().to_string();
        assert!(err.contains("\"Verdict\""), "{err}");
        assert!(err.contains("verdict, comment, people"), "{err}");
    }

    #[test]
    fn only_update_and_install_are_read_only() {
        let read_only: Vec<_> = Section::ALL
            .into_iter()
            .filter(|s| !s.is_tester_writable())
            .collect();
        assert_eq!(read_only, [Section::Update, Section::Install]);
    }

    // --- null_pointers ---

    #[test]
    fn null_pointers_sorts_keys_and_escapes_tokens() {
        let value = json!({
            "b": null,
            "a": {"z": null, "m~n": null, "x/y": null},
            "list": [1, null, {"k": null}],
            "full": 1,
        });
        assert_eq!(
            null_pointers(&value, ""),
            ["/a/m~0n", "/a/x~1y", "/a/z", "/b", "/list/1", "/list/2/k"]
        );
    }

    #[test]
    fn null_pointers_of_a_root_null_is_the_prefix() {
        assert_eq!(null_pointers(&Value::Null, ""), [""]);
        assert_eq!(null_pointers(&Value::Null, "/verdict"), ["/verdict"]);
        assert!(null_pointers(&json!("x"), "/p").is_empty());
    }

    // --- summaries and completeness ---

    #[test]
    fn a_section_the_document_lacks_is_missing_from_the_summaries() {
        let pi = doc(PI);
        assert!(pi.section(Section::Review).is_none());
        let names: Vec<_> = pi.section_summaries().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "verdict", "comment", "people", "update", "install", "issues", "testing"
            ]
        );
    }

    #[test]
    fn summaries_report_size_and_unanswered_per_section() {
        let obs = doc(MAINTENANCE_OBS);
        let rows = obs.section_summaries();
        assert_eq!(rows.len(), 8);
        for row in &rows {
            let section: Section = row.name.parse().unwrap();
            let value = section_of(&obs, section);
            assert_eq!(row.size, serde_json::to_vec(&value).unwrap().len());
            assert_eq!(row.has_unanswered, !null_pointers(&value, "").is_empty());
        }
        let unanswered: Vec<_> = rows
            .iter()
            .filter(|r| r.has_unanswered)
            .map(|r| r.name)
            .collect();
        assert_eq!(
            unanswered,
            ["verdict", "comment", "people", "issues", "review"]
        );
    }

    #[test]
    fn completeness_lists_every_null_leaf_in_sorted_key_order() {
        let obs = doc(MAINTENANCE_OBS);
        let got = obs.completeness();
        assert!(!got.complete);
        assert_eq!(
            got.unfilled,
            [
                "/comment",
                "/issues/bnc#1276308/comment",
                "/issues/bnc#1276308/reproducer",
                "/issues/bnc#1276308/status",
                "/people/reviewer/name",
                "/review/build_log/comment",
                "/review/build_log/test_suite_sufficient",
                "/review/source/comment",
                "/review/source/untracked_changes",
                "/verdict",
            ]
        );
    }

    #[test]
    fn a_document_with_no_null_leaf_is_complete() {
        let got = doc(MAXIMAL).completeness();
        assert!(got.complete, "{:?}", got.unfilled);
        assert!(got.unfilled.is_empty());
    }

    // --- with_section ---

    #[test]
    fn a_valid_testing_write_changes_only_that_section() {
        let before = doc(MAINTENANCE_OBS);
        let value =
            json!({"an_extra_key": "kept", "regression": {"verdict": "PASSED", "comment": null}});
        let after = before
            .with_section(Section::Testing, value.clone())
            .unwrap();

        assert_eq!(section_of(&after, Section::Testing), value);
        for section in Section::ALL.into_iter().filter(|s| *s != Section::Testing) {
            assert_eq!(
                serde_json::to_vec(&section_of(&after, section)).unwrap(),
                serde_json::to_vec(&section_of(&before, section)).unwrap(),
                "{section} changed"
            );
        }
    }

    #[test]
    fn a_scalar_section_write_lands() {
        let after = doc(MAINTENANCE_OBS)
            .with_section(Section::Verdict, json!("FAILED"))
            .unwrap();
        assert_eq!(section_of(&after, Section::Verdict), json!("FAILED"));
    }

    #[test]
    fn pipeline_owned_sections_are_read_only() {
        let obs = doc(MAINTENANCE_OBS);
        for section in [Section::Update, Section::Install] {
            let current = section_of(&obs, section);
            let err = obs.with_section(section, current).unwrap_err();
            assert!(
                matches!(err, SectionWriteError::ReadOnly(s) if s == section),
                "{err}"
            );
        }
    }

    #[test]
    fn a_section_absent_from_the_document_cannot_be_written() {
        let err = doc(PI)
            .with_section(Section::Review, json!({}))
            .unwrap_err();
        assert!(
            matches!(err, SectionWriteError::Absent(Section::Review)),
            "{err}"
        );
    }

    #[test]
    fn a_value_outside_the_enum_is_invalid_and_names_its_pointer() {
        let err = doc(MAINTENANCE_OBS)
            .with_section(Section::Verdict, json!("MAYBE"))
            .unwrap_err();
        match err {
            SectionWriteError::Invalid(e) => assert_eq!(e.pointer, "/verdict"),
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn an_unknown_key_in_a_closed_object_is_dropped_and_refused() {
        let mut people = section_of(&doc(MAINTENANCE_OBS), Section::People);
        people["reviewer"]["bogus"] = json!(1);
        let err = doc(MAINTENANCE_OBS)
            .with_section(Section::People, people)
            .unwrap_err();
        match err {
            SectionWriteError::Dropped(pointers) => {
                assert_eq!(pointers, ["/people/reviewer/bogus"]);
            }
            other => panic!("expected Dropped, got {other}"),
        }
    }

    #[test]
    fn an_unknown_key_in_the_open_testing_object_is_preserved() {
        let after = doc(MAINTENANCE_OBS)
            .with_section(Section::Testing, json!({"brand_new": [1, 2]}))
            .unwrap();
        assert_eq!(
            section_of(&after, Section::Testing),
            json!({"brand_new": [1, 2]})
        );
    }

    #[test]
    fn null_on_an_optional_field_is_dropped_and_refused() {
        let obs = doc(MAINTENANCE_OBS);
        let mut issues = section_of(&obs, Section::Issues);
        issues["bnc#1276308"]["severity"] = Value::Null;
        let err = obs.with_section(Section::Issues, issues).unwrap_err();
        match err {
            SectionWriteError::Dropped(pointers) => {
                assert_eq!(pointers, ["/issues/bnc#1276308/severity"]);
            }
            other => panic!("expected Dropped, got {other}"),
        }
    }

    #[test]
    fn an_issues_write_that_adds_or_drops_a_key_is_refused() {
        let obs = doc(MAINTENANCE_OBS);
        let mut added = section_of(&obs, Section::Issues);
        added["bsc#1"] = added["bnc#1276308"].clone();
        match obs.with_section(Section::Issues, added).unwrap_err() {
            SectionWriteError::IssueKeysChanged { added, removed } => {
                assert_eq!(added, ["bsc#1"]);
                assert!(removed.is_empty());
            }
            other => panic!("expected IssueKeysChanged, got {other}"),
        }

        match obs.with_section(Section::Issues, json!({})).unwrap_err() {
            SectionWriteError::IssueKeysChanged { added, removed } => {
                assert!(added.is_empty());
                assert_eq!(removed, ["bnc#1276308"]);
            }
            other => panic!("expected IssueKeysChanged, got {other}"),
        }
    }

    #[test]
    fn a_non_object_issues_write_is_invalid_not_a_key_change() {
        let err = doc(MAINTENANCE_OBS)
            .with_section(Section::Issues, json!([1]))
            .unwrap_err();
        assert!(matches!(err, SectionWriteError::Invalid(_)), "{err}");
    }

    // --- with_issue ---

    #[test]
    fn an_unknown_issue_is_refused() {
        let err = doc(MAINTENANCE_OBS)
            .with_issue("bsc#999", json!({}))
            .unwrap_err();
        assert!(
            matches!(err, SectionWriteError::UnknownIssue(ref id) if id == "bsc#999"),
            "{err}"
        );
    }

    #[test]
    fn a_malformed_issue_key_is_refused() {
        let err = doc(MAINTENANCE_OBS)
            .with_issue("cve#1", json!({}))
            .unwrap_err();
        assert!(matches!(err, SectionWriteError::BadIssueKey(_)), "{err}");
    }

    #[test]
    fn a_valid_issue_edit_leaves_the_other_issues_byte_equal() {
        let before = doc(MAXIMAL);
        let key = before.issues.keys().next().unwrap().to_string();
        let mut issue = section_of(&before, Section::Issues)[&key].clone();
        issue["status"] = json!("NOT_FIXED");

        let after = before.with_issue(&key, issue).unwrap();

        assert_eq!(
            section_of(&after, Section::Issues)[&key]["status"],
            json!("NOT_FIXED")
        );
        let (mut a, mut b) = (
            section_of(&after, Section::Issues),
            section_of(&before, Section::Issues),
        );
        a.as_object_mut().unwrap().remove(&key);
        b.as_object_mut().unwrap().remove(&key);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
    }

    #[test]
    fn an_issue_edit_that_does_not_fit_the_schema_is_refused() {
        let before = doc(MAXIMAL);
        let key = before.issues.keys().next().unwrap().to_string();
        let mut issue = section_of(&before, Section::Issues)[&key].clone();
        issue["status"] = json!("WHATEVER");
        let err = before.with_issue(&key, issue).unwrap_err();
        match err {
            SectionWriteError::Invalid(e) => assert_eq!(e.pointer, format!("/issues/{key}/status")),
            other => panic!("expected Invalid, got {other}"),
        }
    }
}
