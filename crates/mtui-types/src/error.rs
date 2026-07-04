//! The `mtui-types` error hierarchy.
//!
//! This is the foundation error module every later crate imports. It mirrors
//! the semantics of upstream `mtui/support/exceptions.py`, but scoped to what
//! Phase 1 (the domain types) actually needs.
//!
//! Only the RRID / Request-Review-ID parse errors live here for now — they are
//! consumed by the RRID parser (see the `rrid` task) and are covered by ported
//! upstream test vectors. The upstream `UpdateError` and `GiteaError` families
//! belong to later phases (`mtui-hosts` / `mtui-datasources`) and will be added
//! as `#[from]` sub-errors when those crates land, so they can be exercised by
//! real tests rather than sitting dead here.

use thiserror::Error;

/// Convenience alias for `Result<T, `[`Error`]`>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error type for the `mtui-types` crate.
///
/// Sub-errors are wrapped via `#[from]` so callers can use `?` and still match
/// on the specific failure category.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A Request Review ID (RRID) failed to parse.
    #[error(transparent)]
    RridParse(#[from] RridParseError),

    /// A request-kind token could not be recognised.
    #[error(transparent)]
    RequestKind(#[from] RequestKindParseError),

    /// An RPM version string could not be parsed.
    #[error(transparent)]
    RpmVersionParse(#[from] RpmVersionParseError),

    /// A `refhosts.yml` document could not be parsed.
    #[error(transparent)]
    RefhostsParse(#[from] RefhostsParseError),

    /// A system's base product mapped to no known release.
    #[error(transparent)]
    UnknownSystem(#[from] crate::system::UnknownSystemError),
}

/// Error produced when a `refhosts.yml` document cannot be parsed.
///
/// Mirrors upstream `Refhosts._parse_refhosts`, which lets a YAML parse
/// failure propagate (`logger.error("failed to parse refhosts.yml"); raise`).
/// The Rust port turns that into a typed error wrapping the underlying
/// `serde_yaml` failure. Note: individual *malformed rows* do not surface here
/// — like upstream `_host_from_dict`, they are dropped (logged) so one bad row
/// never aborts the whole load. Only a document-level YAML failure is fatal.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RefhostsParseError {
    /// The YAML document itself was malformed.
    #[error("failed to parse refhosts.yml: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// Error produced when an RPM version string cannot be parsed.
///
/// Mirrors upstream `RPMVersion.__init__` raising a bare `ValueError` for an
/// empty (or `None`) version string. The Rust port turns this into a typed,
/// fallible parse error rather than a panic, following the crate's
/// fallible-constructor convention (see [`RridParseError`]).
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpmVersionParseError {
    /// The version string was empty.
    #[error("RPM version: empty version string")]
    Empty,
}

/// Error produced when a request-kind token is not recognised.
///
/// Mirrors upstream `RequestKind.from_token` raising
/// `ValueError(f"unknown request kind: {raw!r}")`. The `{raw:?}` debug
/// formatting reproduces the Python `!r` repr (quoted token).
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("unknown request kind: {raw:?}")]
pub struct RequestKindParseError {
    /// The raw token that failed to parse.
    pub raw: String,
}

/// Errors produced while parsing an OBS Request Review ID (RRID).
///
/// Mirrors upstream `RequestReviewIDParseError` and its subclasses. Every
/// message is rendered with the upstream `"OBS Request Review ID: "` prefix so
/// the user-facing text remains a stable contract across the Python and Rust
/// implementations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RridParseError {
    /// The RRID had more `:`-separated components than allowed.
    ///
    /// Mirrors upstream `TooManyComponentsError`.
    #[error("OBS Request Review ID: Too many components (> {limit})")]
    TooManyComponents {
        /// The maximum number of components allowed.
        limit: usize,
    },

    /// A required component was absent.
    ///
    /// Mirrors upstream `MissingComponentError`.
    #[error("OBS Request Review ID: Missing {index}. component. Expected: {expected}")]
    MissingComponent {
        /// 1-based index of the missing component.
        index: usize,
        /// Human-readable description of what was expected.
        expected: String,
    },

    /// A component was present but could not be parsed.
    ///
    /// Mirrors upstream `ComponentParseError`.
    #[error(
        "OBS Request Review ID: Failed to parse {index}. component. Expected {expected}. Got: {got:?}"
    )]
    ComponentParse {
        /// 1-based index of the component that failed to parse.
        index: usize,
        /// Human-readable description of what was expected.
        expected: String,
        /// The raw value that was received.
        got: String,
    },

    /// An internal invariant was violated while parsing.
    ///
    /// Mirrors upstream `InternalParseError`.
    #[error("OBS Request Review ID: Internal error: f: {func:?} cnt: {count:?}")]
    Internal {
        /// The parsing step / function where the error occurred.
        func: String,
        /// The context value at the point of failure.
        count: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_many_components_matches_upstream_message() {
        let err = RridParseError::TooManyComponents { limit: 4 };
        assert_eq!(
            err.to_string(),
            "OBS Request Review ID: Too many components (> 4)"
        );
    }

    #[test]
    fn missing_component_matches_upstream_message() {
        let err = RridParseError::MissingComponent {
            index: 2,
            expected: "maintenance_id".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "OBS Request Review ID: Missing 2. component. Expected: maintenance_id"
        );
    }

    #[test]
    fn component_parse_matches_upstream_message() {
        let err = RridParseError::ComponentParse {
            index: 3,
            expected: "an integer".to_owned(),
            got: "abc".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "OBS Request Review ID: Failed to parse 3. component. Expected an integer. Got: \"abc\""
        );
    }

    #[test]
    fn internal_matches_upstream_message() {
        let err = RridParseError::Internal {
            func: "split".to_owned(),
            count: "0".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "OBS Request Review ID: Internal error: f: \"split\" cnt: \"0\""
        );
    }

    #[test]
    fn from_rrid_parse_error_wraps_and_displays_transparently() {
        let rrid = RridParseError::TooManyComponents { limit: 4 };
        let err: Error = rrid.clone().into();
        // `#[error(transparent)]` means the wrapper's Display equals the inner's.
        assert_eq!(err.to_string(), rrid.to_string());
        assert!(matches!(err, Error::RridParse(_)));
    }

    #[test]
    fn transparent_wrapper_delegates_source_to_inner() {
        use std::error::Error as _;
        // `#[error(transparent)]` forwards `source()` to the inner error. The
        // inner `RridParseError` is a leaf (no nested source), so the wrapper
        // reports `None` — proving the wrapper adds no spurious layer.
        let err: Error = RridParseError::TooManyComponents { limit: 4 }.into();
        assert!(err.source().is_none());
    }

    #[test]
    fn rpm_version_empty_matches_message() {
        let err = RpmVersionParseError::Empty;
        assert_eq!(err.to_string(), "RPM version: empty version string");
    }

    #[test]
    fn from_rpm_version_parse_error_wraps_transparently() {
        let inner = RpmVersionParseError::Empty;
        let err: Error = inner.clone().into();
        assert_eq!(err.to_string(), inner.to_string());
        assert!(matches!(err, Error::RpmVersionParse(_)));
    }

    #[test]
    fn rrid_parse_error_equality() {
        assert_eq!(
            RridParseError::MissingComponent {
                index: 1,
                expected: "project".to_owned()
            },
            RridParseError::MissingComponent {
                index: 1,
                expected: "project".to_owned()
            }
        );
    }
}
