//! Product / addon version, ported from `mtui/hosts/refhost/models.py::Version`.
//!
//! A version has a `major` and an optional `minor`. Both fields may be either
//! numeric (`15`, `5`) or textual (`sp4`). Upstream deliberately preserves this
//! distinction — `isinstance(minor, int)` drives the `.` vs `""`/`-` separator
//! in the refhost formatters — so the Rust port models each field as a
//! [`VersionField`] enum rather than collapsing everything to a string.
//!
//! `minor == ""` is an upstream sentinel used in *queries* meaning "match
//! candidates that have no minor set"; in *candidates* loaded from
//! `refhosts.yml`, an omitted `minor` key deserializes to `None`.

use serde::{Deserialize, Serialize};

/// A single version field, preserving the numeric-vs-textual distinction.
///
/// YAML `5` deserializes to [`VersionField::Num`]; YAML `sp4` deserializes to
/// [`VersionField::Text`]. The `Num` variant is listed first so `#[serde(untagged)]`
/// prefers the integer interpretation for bare numeric scalars.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VersionField {
    /// A numeric field (e.g. `15`, `5`).
    Num(u64),
    /// A textual field (e.g. `sp4`, or the empty-string query sentinel).
    Text(String),
}

impl std::fmt::Display for VersionField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Num(n) => write!(f, "{n}"),
            Self::Text(s) => f.write_str(s),
        }
    }
}

impl From<u64> for VersionField {
    fn from(n: u64) -> Self {
        Self::Num(n)
    }
}

impl From<String> for VersionField {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for VersionField {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

/// A product or addon version (`major` plus optional `minor`).
///
/// Mirrors upstream `Version` (a frozen dataclass). Deserializes directly from
/// the `refhosts.yml` `version:` mapping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Version {
    /// The major version component (numeric or textual).
    pub major: VersionField,
    /// The optional minor version component (numeric like `5`, textual like
    /// `sp4`, or absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<VersionField>,
}

impl Version {
    /// Constructs a version from any [`VersionField`]-convertible values.
    pub fn new(major: impl Into<VersionField>, minor: Option<VersionField>) -> Self {
        Self {
            major: major.into(),
            minor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_minor_round_trips_as_num() {
        let yaml = "major: 15\nminor: 5\n";
        let v: Version = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v.major, VersionField::Num(15));
        assert_eq!(v.minor, Some(VersionField::Num(5)));
        // Re-serializing preserves the numeric shape.
        let round = serde_yaml::to_string(&v).unwrap();
        let back: Version = serde_yaml::from_str(&round).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn textual_minor_round_trips_as_text() {
        let yaml = "major: 12\nminor: sp4\n";
        let v: Version = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v.major, VersionField::Num(12));
        assert_eq!(v.minor, Some(VersionField::Text("sp4".to_owned())));
        let round = serde_yaml::to_string(&v).unwrap();
        let back: Version = serde_yaml::from_str(&round).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn omitted_minor_deserializes_to_none() {
        let yaml = "major: 15\n";
        let v: Version = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v.major, VersionField::Num(15));
        assert_eq!(v.minor, None);
        // With minor None, it is skipped on serialize.
        let round = serde_yaml::to_string(&v).unwrap();
        assert!(!round.contains("minor"));
    }

    #[test]
    fn numeric_and_textual_minor_are_distinct() {
        let num: Version = serde_yaml::from_str("major: 12\nminor: 4\n").unwrap();
        let text: Version = serde_yaml::from_str("major: 12\nminor: sp4\n").unwrap();
        assert_ne!(num.minor, text.minor);
    }

    #[test]
    fn display_renders_each_field() {
        assert_eq!(VersionField::Num(15).to_string(), "15");
        assert_eq!(VersionField::Text("sp4".to_owned()).to_string(), "sp4");
    }

    #[test]
    fn new_constructor_builds_expected_version() {
        let v = Version::new(15u64, Some("sp2".into()));
        assert_eq!(v.major, VersionField::Num(15));
        assert_eq!(v.minor, Some(VersionField::Text("sp2".to_owned())));
    }
}
