//! Referenced bugs (`issues`, `$defs/issue`, `$defs/l3` of the TeReGen
//! report-document schema, v1.0).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::Req;

/// `issues`: bugs and Jira features keyed `<tracker>#<number>`, excluding
/// CVEs.
pub type Issues = BTreeMap<IssueKey, Issue>;

/// The five bug-tracker prefixes the schema's `issues` `propertyNames`
/// pattern allows.
const VALID_PREFIXES: [&str; 5] = ["bsc", "bnc", "boo", "jsc", "ijsc"];

/// A validated `issues` map key, e.g. `bsc#1234567`
/// (`^(bsc|bnc|boo|jsc|ijsc)#[A-Za-z0-9-]+$`). Parsing — and therefore
/// deserializing — an invalid key is a typed error rather than a `422` on the
/// next `PUT`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IssueKey(String);

impl IssueKey {
    /// Returns the validated key string, e.g. `"bsc#1234567"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Error returned when an issue key does not match
/// `^(bsc|bnc|boo|jsc|ijsc)#[A-Za-z0-9-]+$`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid issue key: {raw:?}")]
pub struct IssueKeyParseError {
    raw: String,
}

impl FromStr for IssueKey {
    type Err = IssueKeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.split_once('#').is_some_and(|(prefix, suffix)| {
            VALID_PREFIXES.contains(&prefix)
                && !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(IssueKeyParseError { raw: s.to_owned() })
        }
    }
}

impl fmt::Display for IssueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for IssueKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IssueKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(D::Error::custom)
    }
}

/// One `issues` entry (`$defs/issue`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    /// Bug summary as the patchinfo states it.
    pub title: String,
    /// Bug severity, absent when no source supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// `const true` in the schema (true only for a bug a tester filed during
    /// validation) — refuses `false` on deserialize, so this type can never
    /// emit the one value the schema forbids.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_new_flag"
    )]
    pub new: Option<bool>,
    /// Whether a reproducer exists, null until answered.
    pub reproducer: Req<bool>,
    /// Validation outcome, null until answered.
    pub status: Req<IssueStatus>,
    /// Tester's free text about this issue.
    pub comment: Req<String>,
    /// SolidGround record, present only for L3-tracked bugs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l3: Option<L3>,
}

fn deserialize_new_flag<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    match Option::<bool>::deserialize(deserializer)? {
        Some(false) => Err(D::Error::custom(
            "issue.new must be true or absent, never false",
        )),
        other => Ok(other),
    }
}

/// Bug severity (`$defs/issue.severity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Major,
    Normal,
    Minor,
    Enhancement,
}

/// Validation outcome (`$defs/issue.status`), required-nullable everywhere it
/// appears — see [`Req<IssueStatus>`] on [`Issue::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueStatus {
    #[serde(rename = "FIXED")]
    Fixed,
    #[serde(rename = "NOT_FIXED")]
    NotFixed,
    #[serde(rename = "HYPOTHETICAL")]
    Hypothetical,
    #[serde(rename = "NOT_REPRODUCIBLE")]
    NotReproducible,
    #[serde(rename = "NO_ENVIRONMENT")]
    NoEnvironment,
    #[serde(rename = "TOO_COMPLEX")]
    TooComplex,
    #[serde(rename = "SKIPPED")]
    Skipped,
    #[serde(rename = "OTHER")]
    Other,
}

/// SolidGround record (`$defs/l3`), omitting the fields upstream left empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L3 {
    pub incident: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_packages: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptf_packages: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<String>,
    /// SolidGround calls this `outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// SolidGround's own wording, such as `Major`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scriptability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thirdparty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproducer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_key_rejects_unknown_tracker_prefix() {
        let err = "cve#1234".parse::<IssueKey>().unwrap_err();
        assert_eq!(err.to_string(), "invalid issue key: \"cve#1234\"");
    }

    #[test]
    fn issue_key_rejects_empty_suffix() {
        assert!("bsc#".parse::<IssueKey>().is_err());
    }

    #[test]
    fn issue_key_accepts_and_exposes_a_valid_key() {
        let key: IssueKey = "bnc#1273133".parse().unwrap();
        assert_eq!(key.as_str(), "bnc#1273133");
        assert_eq!(key.to_string(), "bnc#1273133");
    }

    #[test]
    fn issue_key_accepts_ijsc() {
        let key: IssueKey = "ijsc#PED-12345".parse().unwrap();
        assert_eq!(key.as_str(), "ijsc#PED-12345");
        assert_eq!(key.to_string(), "ijsc#PED-12345");
    }

    #[test]
    fn issue_key_rejects_prefixes_that_merely_contain_ijsc() {
        assert!("ijsc2#1".parse::<IssueKey>().is_err());
        assert!("zsc#1".parse::<IssueKey>().is_err());
    }

    #[test]
    fn issue_new_false_is_a_deserialize_error() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "deserialize_new_flag")]
            new: Option<bool>,
        }
        assert!(serde_json::from_str::<Holder>(r#"{"new": false}"#).is_err());
        let ok: Holder = serde_json::from_str(r#"{"new": true}"#).unwrap();
        assert_eq!(ok.new, Some(true));
    }
}
