//! OBS Request Review ID (RRID).
//!
//! An RRID is the `project:kind:maintenance_id:review_id` identifier that names
//! a maintenance request across the SUSE ecosystem. Its grammar and parse errors
//! are an interop **Contract** (see `AGENTS.md`): RRIDs are minted by OBS/IBS
//! and the rendered form becomes the on-disk testreport directory name, so
//! loosening the grammar admits identifiers the ecosystem rejects and tightening
//! it strands checkouts already on disk — either way a breaking change.
//!
//! ## Grammar
//!
//! The string is split on `:` with empty tokens dropped, then exactly four
//! components are parsed positionally (more than four is rejected as too many;
//! fewer than four leaves a required component absent):
//!
//! 1. **project** — one of `SUSE` / `S`; the short form `S` normalises to `SUSE`.
//! 2. **kind** — one of `SLFO` / `S` / `Maintenance` / `M` / `PI` / `P`, mapped
//!    to a [`RequestKind`] via `RequestKind::from_token`.
//! 3. **maintenance_id** — any non-empty token (an integer, or a string
//!    fallback, so every non-empty token parses). Stored as the raw token
//!    string.
//! 4. **review_id** — must parse as an integer.
//!
//! A missing component yields [`RridParseError::MissingComponent`]; a component
//! that fails its parser yields [`RridParseError::ComponentParse`]; more than
//! four components yields [`RridParseError::TooManyComponents`].
//!
//! Equality and hashing are structural: the parser normalises `project`
//! (`S` → `SUSE`) and canonicalises `kind` (`M` → `Maintenance`), so
//! `S:M:1:1` compares equal to `SUSE:Maintenance:1:1`.

use std::fmt;
use std::str::FromStr;

use crate::enums::RequestKind;
use crate::error::RridParseError;

/// The exact number of components a well-formed RRID must have
/// (`project:kind:maintenance_id:review_id`). Enforced as two bounds: more
/// tokens is rejected as too many, fewer leaves trailing parsers with no token
/// and each raises a missing-component error.
const REQUIRED_COMPONENTS: usize = 4;

/// A parsed OBS Request Review ID.
///
/// Construct one with [`RequestReviewID::parse`] or via [`FromStr`]. Fields are
/// normalised on parse (`project` short form expanded, `kind` canonicalised),
/// so structural equality has string-identity semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestReviewID {
    /// The project, always the canonical long form (`SUSE`).
    pub project: String,
    /// The request kind.
    pub kind: RequestKind,
    /// The maintenance ID, stored as the raw token (an integer for
    /// Maintenance/PI kinds, a dotted string such as `1.1` for SLFO).
    pub maintenance_id: String,
    /// The review ID (always an integer).
    pub review_id: u64,
}

impl RequestReviewID {
    /// Parses a fully qualified Request Review ID string.
    ///
    /// # Errors
    ///
    /// Returns a [`RridParseError`] for a parse failure:
    /// [`TooManyComponents`](RridParseError::TooManyComponents) for more than
    /// four components, [`MissingComponent`](RridParseError::MissingComponent)
    /// for an absent component, and
    /// [`ComponentParse`](RridParseError::ComponentParse) for a component that
    /// fails its parser (unknown project/kind, or a non-integer review ID).
    pub fn parse(rrid: &str) -> Result<Self, RridParseError> {
        // Dropping empty tokens ignores leading/trailing/doubled colons.
        let tokens: Vec<&str> = rrid.split(':').filter(|t| !t.is_empty()).collect();

        // Too few is not rejected here: the trailing components come back
        // absent below and each raises `MissingComponent`.
        if tokens.len() > REQUIRED_COMPONENTS {
            return Err(RridParseError::TooManyComponents {
                limit: REQUIRED_COMPONENTS,
            });
        }

        let project = parse_project(component(&tokens, 0), 1)?;
        let kind = parse_kind(component(&tokens, 1), 2)?;
        let maintenance_id = parse_maintenance_id(component(&tokens, 2), 3)?;
        let review_id = parse_review_id(component(&tokens, 3), 4)?;

        Ok(Self {
            project,
            kind,
            maintenance_id,
            review_id,
        })
    }
}

/// The token at `idx`, or `None` when absent — a short input yields `None` for
/// the trailing parsers.
fn component<'a>(tokens: &[&'a str], idx: usize) -> Option<&'a str> {
    tokens.get(idx).copied()
}

/// Component 1 — project. Accepts `SUSE` or `S`, with `S` → `SUSE`.
fn parse_project(token: Option<&str>, index: usize) -> Result<String, RridParseError> {
    let raw = require(token, index, "one of SUSE, S")?;
    match raw {
        "SUSE" => Ok("SUSE".to_owned()),
        "S" => Ok("SUSE".to_owned()),
        other => Err(RridParseError::ComponentParse {
            index,
            expected: "one of SUSE, S".to_owned(),
            got: other.to_owned(),
        }),
    }
}

/// Component 2 — kind. Mapped to a [`RequestKind`] via [`RequestKind::from_token`].
fn parse_kind(token: Option<&str>, index: usize) -> Result<RequestKind, RridParseError> {
    let raw = require(token, index, "one of SLFO, S, Maintenance, M, PI, P")?;
    RequestKind::from_token(raw).map_err(|_| RridParseError::ComponentParse {
        index,
        expected: "one of SLFO, S, Maintenance, M, PI, P".to_owned(),
        got: raw.to_owned(),
    })
}

/// Component 3 — maintenance ID. Any non-empty token parses, so the only
/// failure is absence. Stored raw to preserve the int-vs-string distinction
/// downstream code depends on (`1` vs `1.1`).
fn parse_maintenance_id(token: Option<&str>, index: usize) -> Result<String, RridParseError> {
    let raw = require(token, index, "an integer or string")?;
    Ok(raw.to_owned())
}

/// Component 4 — review ID. Requires an integer.
fn parse_review_id(token: Option<&str>, index: usize) -> Result<u64, RridParseError> {
    let raw = require(token, index, "an integer")?;
    raw.parse::<u64>()
        .map_err(|_| RridParseError::ComponentParse {
            index,
            expected: "an integer".to_owned(),
            got: raw.to_owned(),
        })
}

/// Missing-value guard: an absent component raises a missing-component error.
fn require<'a>(
    token: Option<&'a str>,
    index: usize,
    expected: &str,
) -> Result<&'a str, RridParseError> {
    token.ok_or_else(|| RridParseError::MissingComponent {
        index,
        expected: expected.to_owned(),
    })
}

impl FromStr for RequestReviewID {
    type Err = RridParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for RequestReviewID {
    /// Renders `project:kind:maintenance_id:review_id`, using the kind's
    /// canonical long form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.project,
            self.kind.as_str(),
            self.maintenance_id,
            self.review_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let rrid = RequestReviewID::parse("SUSE:Maintenance:1:2").unwrap();
        assert_eq!(rrid.to_string(), "SUSE:Maintenance:1:2");
    }

    #[test]
    fn short_project_normalises_to_suse() {
        let rrid = RequestReviewID::parse("S:M:1:2").unwrap();
        assert_eq!(rrid.project, "SUSE");
        assert_eq!(rrid.kind, RequestKind::Maintenance);
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let rrid: RequestReviewID = "SUSE:Maintenance:1:2".parse().unwrap();
        assert_eq!(rrid.review_id, 2);
    }
}
