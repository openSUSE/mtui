//! Domain enumerations, ported from upstream `mtui/types/enums.py`.
//!
//! Only the enums with existing behavioral coverage (`tests/test_enums.py`)
//! or a refhost consumer are ported here. Upstream's `method` / `assignment`
//! HTTP-layer enums are intentionally deferred until a caller lands, to keep
//! this crate free of dead code under `-D warnings`.
//!
//! Where upstream relies on Python's `StrEnum` for byte-identical string
//! comparison (`target.state == "enabled"`), the Rust equivalent is
//! `#[serde(rename = ...)]` on the wire form plus `Display`/`FromStr` that
//! preserve the exact upstream strings — this keeps the CLI/config/serialized
//! surface a stable contract without leaking a `str`-equality footgun.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RequestKindParseError;

/// Per-host execution state.
///
/// Mirrors upstream `TargetState` (a `StrEnum`). Wire values are the exact
/// lowercase tokens `enabled` / `dryrun` / `disabled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetState {
    /// The host runs commands normally.
    Enabled,
    /// The host echoes commands without executing them.
    Dryrun,
    /// The host is skipped entirely.
    Disabled,
}

impl TargetState {
    /// Returns the upstream wire string for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Dryrun => "dryrun",
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
            "dryrun" => Ok(Self::Dryrun),
            "disabled" => Ok(Self::Disabled),
            other => Err(ParseEnumError {
                kind: "TargetState",
                got: other.to_owned(),
            }),
        }
    }
}

/// Whether a host runs commands in parallel with its group or under a serial
/// barrier.
///
/// Mirrors upstream `ExecutionMode` (a plain `Enum`). Wire values are
/// `parallel` / `serial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    /// The host runs concurrently with the rest of its group.
    Parallel,
    /// The host holds the group in a serial barrier.
    Serial,
}

impl ExecutionMode {
    /// Returns the upstream wire string for this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Serial => "serial",
        }
    }
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ExecutionMode {
    type Err = ParseEnumError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "parallel" => Ok(Self::Parallel),
            "serial" => Ok(Self::Serial),
            other => Err(ParseEnumError {
                kind: "ExecutionMode",
                got: other.to_owned(),
            }),
        }
    }
}

/// Per-report update workflow mode.
///
/// Mirrors upstream `Workflow` (a `StrEnum`). Wire values match the
/// `set_workflow` CLI choices `auto` / `manual` / `kernel`.
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
    /// Returns the upstream wire string for this workflow.
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
/// Mirrors upstream `RequestKind` (a plain `Enum`). The canonical wire values
/// are `SLFO` / `Maintenance` / `PI`; [`RequestKind::from_token`] also accepts
/// the single-letter CLI aliases `S` / `M` / `P`.
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

    /// Parse the short or long form of a request kind.
    ///
    /// Accepts the single-letter aliases (`S` / `M` / `P`) used on the command
    /// line and the canonical long forms (`SLFO` / `Maintenance` / `PI`) used
    /// in the wire format.
    ///
    /// # Errors
    ///
    /// Returns [`RequestKindParseError`] if `raw` is not a recognised kind,
    /// mirroring upstream `ValueError("unknown request kind: …")`.
    pub fn from_token(raw: &str) -> Result<Self, RequestKindParseError> {
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

    // --- TargetState: upstream `StrEnum` string-value contract. ---

    #[test]
    fn target_state_carries_legacy_string_values() {
        assert_eq!(TargetState::Enabled.as_str(), "enabled");
        assert_eq!(TargetState::Dryrun.as_str(), "dryrun");
        assert_eq!(TargetState::Disabled.as_str(), "disabled");
    }

    #[test]
    fn target_state_round_trips_through_str() {
        for state in [
            TargetState::Enabled,
            TargetState::Dryrun,
            TargetState::Disabled,
        ] {
            assert_eq!(state.to_string().parse::<TargetState>().unwrap(), state);
        }
    }

    #[test]
    fn target_state_serde_uses_wire_strings() {
        let json = serde_json::to_string(&TargetState::Dryrun).unwrap();
        assert_eq!(json, "\"dryrun\"");
        let back: TargetState = serde_json::from_str("\"disabled\"").unwrap();
        assert_eq!(back, TargetState::Disabled);
    }

    #[test]
    fn target_state_rejects_unknown() {
        let err = "bogus".parse::<TargetState>().unwrap_err();
        assert_eq!(err.kind, "TargetState");
        assert_eq!(err.got, "bogus");
    }

    // --- ExecutionMode. ---

    #[test]
    fn execution_mode_string_values_and_parse() {
        assert_eq!(ExecutionMode::Parallel.as_str(), "parallel");
        assert_eq!(ExecutionMode::Serial.as_str(), "serial");
        assert_eq!(
            "parallel".parse::<ExecutionMode>().unwrap(),
            ExecutionMode::Parallel
        );
        assert_eq!(
            "serial".parse::<ExecutionMode>().unwrap(),
            ExecutionMode::Serial
        );
    }

    #[test]
    fn execution_mode_rejects_unknown() {
        assert!("nope".parse::<ExecutionMode>().is_err());
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

    // --- RequestKind: ported from tests/test_enums.py::TestRequestKind. ---

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
