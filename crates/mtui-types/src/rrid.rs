//! OBS Request Review ID (RRID), ported from `mtui/types/rrid.py`.
//!
//! An RRID is the `project:kind:maintenance_id:review_id` identifier that names
//! a maintenance request across the SUSE ecosystem. Its grammar and parse
//! errors are an interop **Contract** (see `AGENTS.md`): a Rust mtui and a
//! Python mtui must agree byte-for-byte on what parses and what fails, so this
//! module mirrors upstream's `RequestReviewID.__init__` component-by-component.
//!
//! ## Grammar (upstream `rrid.py`)
//!
//! The string is split on `:` with empty tokens dropped, then exactly four
//! components are parsed positionally (more than four is rejected as too many;
//! fewer than four leaves a required component absent):
//!
//! 1. **project** — one of `SUSE` / `S`; the short form `S` normalises to `SUSE`.
//! 2. **kind** — one of `SLFO` / `S` / `Maintenance` / `M` / `PI` / `P`, mapped
//!    to a [`RequestKind`] via [`RequestKind::from_token`].
//! 3. **maintenance_id** — any non-empty token (upstream `check_type(int, str)`
//!    accepts an integer or, failing that, a string; every non-empty token
//!    therefore parses). Stored as the raw token string.
//! 4. **review_id** — must parse as an integer (upstream `check_type(int)`).
//!
//! A missing component yields [`RridParseError::MissingComponent`]; a component
//! that fails its parser yields [`RridParseError::ComponentParse`]; more than
//! four components yields [`RridParseError::TooManyComponents`].
//!
//! Equality and hashing are structural: the parser normalises `project`
//! (`S` → `SUSE`) and canonicalises `kind` (`M` → `Maintenance`), so
//! `S:M:1:1` compares equal to `SUSE:Maintenance:1:1` — matching upstream's
//! string-identity `__eq__` / `__hash__`.

use std::fmt;
use std::str::FromStr;

use crate::enums::RequestKind;
use crate::error::RridParseError;

/// The exact number of components a well-formed RRID must have
/// (`project:kind:maintenance_id:review_id`).
///
/// Upstream enforces this as two bounds: more than this many tokens raises
/// `TooManyComponentsError`, and fewer than this many leaves trailing parsers
/// with no token (via `zip_longest`), each raising `MissingComponentError`.
const REQUIRED_COMPONENTS: usize = 4;

/// A parsed OBS Request Review ID.
///
/// Construct one with [`RequestReviewID::parse`] or via [`FromStr`]. Fields are
/// normalised on parse (`project` short form expanded, `kind` canonicalised),
/// so structural equality matches upstream's string-identity semantics.
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
    /// Returns a [`RridParseError`] mirroring upstream's parse failures:
    /// [`TooManyComponents`](RridParseError::TooManyComponents) for more than
    /// four components, [`MissingComponent`](RridParseError::MissingComponent)
    /// for an absent component, and
    /// [`ComponentParse`](RridParseError::ComponentParse) for a component that
    /// fails its parser (unknown project/kind, or a non-integer review ID).
    pub fn parse(rrid: &str) -> Result<Self, RridParseError> {
        // Upstream: `[x for x in rrid.split(":") if x]` — split on ':' and drop
        // empty tokens (so leading/trailing/doubled colons are ignored).
        let tokens: Vec<&str> = rrid.split(':').filter(|t| !t.is_empty()).collect();

        // Upstream: `TooManyComponentsError.raise_if(xs, 4)`. Fewer than the
        // required count is not rejected here — the trailing components come
        // back absent below and each raises `MissingComponent`, mirroring
        // upstream's `zip_longest` padding.
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

/// Returns the token at `idx`, or `None` when it is absent (a missing
/// component). Upstream models this via `zip_longest`, where a short input
/// yields `None` for the trailing parsers.
fn component<'a>(tokens: &[&'a str], idx: usize) -> Option<&'a str> {
    tokens.get(idx).copied()
}

/// Component 1 — project. Upstream `check_eq("SUSE", "S")` with `S` → `SUSE`.
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

/// Component 2 — kind. Upstream `check_eq(...)` then `RequestKind.from_token`.
fn parse_kind(token: Option<&str>, index: usize) -> Result<RequestKind, RridParseError> {
    let raw = require(token, index, "one of SLFO, S, Maintenance, M, PI, P")?;
    RequestKind::from_token(raw).map_err(|_| RridParseError::ComponentParse {
        index,
        expected: "one of SLFO, S, Maintenance, M, PI, P".to_owned(),
        got: raw.to_owned(),
    })
}

/// Component 3 — maintenance ID. Upstream `check_type(int, str)` accepts any
/// non-empty token (an integer, or a string fallback), so the only failure is
/// absence. Stored as the raw token to preserve the int-vs-string distinction
/// downstream code depends on (`1` vs `1.1`).
fn parse_maintenance_id(token: Option<&str>, index: usize) -> Result<String, RridParseError> {
    let raw = require(token, index, "an integer or string")?;
    Ok(raw.to_owned())
}

/// Component 4 — review ID. Upstream `check_type(int)` requires an integer.
fn parse_review_id(token: Option<&str>, index: usize) -> Result<u64, RridParseError> {
    let raw = require(token, index, "an integer")?;
    raw.parse::<u64>()
        .map_err(|_| RridParseError::ComponentParse {
            index,
            expected: "an integer".to_owned(),
            got: raw.to_owned(),
        })
}

/// Mirrors upstream `apply_parser`'s missing-value guard: an absent component
/// (`not x`) raises `MissingComponentError(cnt, f)`.
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
    /// Renders `project:kind:maintenance_id:review_id`, matching upstream
    /// `__str__` (which uses `kind.value`, the canonical long form).
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
