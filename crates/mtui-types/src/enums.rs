//! Domain enumerations. Only enums with a live consumer are defined here, so
//! the crate stays free of dead code under `-D warnings`.
//!
//! Wire values must be byte-identical strings for the CLI/config/serialised
//! surface (e.g. `target.state == "enabled"`): `#[serde(rename = ...)]` plus
//! `Display`/`FromStr` preserving those exact strings keep that a stable
//! contract without leaking a `str`-equality footgun.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RequestKindParseError;

/// Per-host execution state.
///
/// Wire values are the exact lowercase tokens `enabled` / `disabled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetState {
    /// The host runs commands normally.
    Enabled,
    /// The host is skipped entirely.
    Disabled,
}

impl TargetState {
    /// Returns the wire string for this state.
    #[must_use]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for TargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TargetState {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "enabled" => Ok(Self::Enabled),
            "disabled" => Ok(Self::Disabled),
            other => Err(ParseEnumError {
                kind: "TargetState",
                got: other.to_owned(),
            }),
        }
    }
}

/// Per-report update workflow mode.
///
/// Wire values match the `set_workflow` CLI choices `auto` / `manual` /
/// `kernel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workflow {
    /// Automatic workflow.
    Auto,
    /// Manual workflow.
    Manual,
    /// Kernel workflow.
    Kernel,
}

impl Workflow {
    /// Returns the wire string for this workflow.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Kernel => "kernel",
        }
    }
}

impl fmt::Display for Workflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Workflow {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "manual" => Ok(Self::Manual),
            "kernel" => Ok(Self::Kernel),
            other => Err(ParseEnumError {
                kind: "Workflow",
                got: other.to_owned(),
            }),
        }
    }
}

/// Kind component of an OBS Request Review ID.
///
/// The canonical wire values are `SLFO` / `Maintenance` / `PI`;
/// `RequestKind::from_token` also accepts the single-letter CLI aliases
/// `S` / `M` / `P`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RequestKind {
    /// SUSE Linux Framework One.
    #[serde(rename = "SLFO")]
    Slfo,
    /// Maintenance update.
    #[serde(rename = "Maintenance")]
    Maintenance,
    /// Product Increment.
    #[serde(rename = "PI")]
    Pi,
}

impl RequestKind {
    /// Returns the canonical wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slfo => "SLFO",
            Self::Maintenance => "Maintenance",
            Self::Pi => "PI",
        }
    }

    /// Parse the short (`S` / `M` / `P`) or canonical long form of a kind.
    ///
    /// # Errors
    ///
    /// Returns [`RequestKindParseError`] if `raw` is not a recognised kind.
    pub(crate) fn from_token(raw: &str) -> Result<Self, RequestKindParseError> {
        match raw {
            "S" | "SLFO" => Ok(Self::Slfo),
            "M" | "Maintenance" => Ok(Self::Maintenance),
            "P" | "PI" => Ok(Self::Pi),
            other => Err(RequestKindParseError {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for RequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Assignment state of a Gitea pull request for a review group, derived by
/// replaying the group's assign/unassign marker comments.
///
/// Purely in-memory state between the connector and its error type (it is the
/// discriminant of the assign-invalid message), so there is no wire-string
/// contract and no serde/`FromStr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Assignment {
    /// The PR's group is assigned to the user under consideration.
    AssignedUser,
    /// The PR's group is not assigned to anyone.
    Unassigned,
    /// The PR's group is assigned to a *different* user.
    AssignedOther,
}

/// Error returned by the [`FromStr`] impls of the string-valued enums.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} value: {got:?}")]
pub struct ParseEnumError {
    /// The name of the enum that failed to parse.
    pub kind: &'static str,
    /// The raw token that was not recognised.
    pub got: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- TargetState. ---

    #[test]
    fn target_state_carries_legacy_string_values() {
        assert_eq!(TargetState::Enabled.as_str(), "enabled");
        assert_eq!(TargetState::Disabled.as_str(), "disabled");
    }

    #[test]
    fn target_state_round_trips_through_str() {
        for state in [TargetState::Enabled, TargetState::Disabled] {
            assert_eq!(state.to_string().parse::<TargetState>().unwrap(), state);
        }
    }

    #[test]
    fn target_state_serde_uses_wire_strings() {
        let json = serde_json::to_string(&TargetState::Enabled).unwrap();
        assert_eq!(json, "\"enabled\"");
        let back: TargetState = serde_json::from_str("\"disabled\"").unwrap();
        assert_eq!(back, TargetState::Disabled);
        assert!(serde_json::from_str::<TargetState>("\"dryrun\"").is_err());
    }

    #[test]
    fn target_state_rejects_unknown() {
        let err = "bogus".parse::<TargetState>().unwrap_err();
        assert_eq!(err.kind, "TargetState");
        assert_eq!(err.got, "bogus");
    }

    // --- Workflow. ---

    #[test]
    fn workflow_string_values_and_parse() {
        assert_eq!(Workflow::Auto.as_str(), "auto");
        assert_eq!(Workflow::Manual.as_str(), "manual");
        assert_eq!(Workflow::Kernel.as_str(), "kernel");
        assert_eq!("kernel".parse::<Workflow>().unwrap(), Workflow::Kernel);
    }

    #[test]
    fn workflow_rejects_unknown() {
        assert!("hybrid".parse::<Workflow>().is_err());
    }

    // --- RequestKind. ---

    #[test]
    fn request_kind_canonical_values_match_wire_format() {
        assert_eq!(RequestKind::Slfo.as_str(), "SLFO");
        assert_eq!(RequestKind::Maintenance.as_str(), "Maintenance");
        assert_eq!(RequestKind::Pi.as_str(), "PI");
    }

    #[test]
    fn request_kind_from_token_accepts_long_and_short_forms() {
        let cases = [
            ("S", RequestKind::Slfo),
            ("SLFO", RequestKind::Slfo),
            ("M", RequestKind::Maintenance),
            ("Maintenance", RequestKind::Maintenance),
            ("P", RequestKind::Pi),
            ("PI", RequestKind::Pi),
        ];
        for (token, expected) in cases {
            assert_eq!(RequestKind::from_token(token).unwrap(), expected);
        }
    }

    #[test]
    fn request_kind_from_token_rejects_unknown() {
        // "SLE" is a historical typo found in fixtures.
        let err = RequestKind::from_token("SLE").unwrap_err();
        assert_eq!(err.raw, "SLE");
        assert_eq!(err.to_string(), "unknown request kind: \"SLE\"");
    }

    // --- Assignment. ---

    #[test]
    fn assignment_variants_are_distinct() {
        assert_ne!(Assignment::AssignedUser, Assignment::Unassigned);
        assert_ne!(Assignment::AssignedUser, Assignment::AssignedOther);
        assert_ne!(Assignment::Unassigned, Assignment::AssignedOther);
        let a = Assignment::AssignedUser;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn request_kind_serde_uses_canonical_wire_values() {
        assert_eq!(
            serde_json::to_string(&RequestKind::Maintenance).unwrap(),
            "\"Maintenance\""
        );
        let back: RequestKind = serde_json::from_str("\"SLFO\"").unwrap();
        assert_eq!(back, RequestKind::Slfo);
    }
}
