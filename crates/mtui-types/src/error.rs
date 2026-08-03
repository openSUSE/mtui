//! The `mtui-types` error hierarchy.
//!
//! This is the foundation error module every later crate imports, scoped to
//! what the domain types actually need.
//!
//! Only the RRID / Request-Review-ID parse errors live here — they are
//! consumed by the RRID parser (see the `rrid` module) and are covered by
//! golden test vectors. `mtui-hosts` and `mtui-datasources` define their own
//! error enums (`HostError`, `GiteaError`, and friends) rather than extending
//! this one via `#[from]`.

use thiserror::Error;

/// Convenience alias for `Result<T, `[`enum@Error`]`>`.
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

    /// A package name/spec failed validation.
    #[error(transparent)]
    PackageSpecParse(#[from] PackageSpecParseError),

    /// A repository URL failed validation.
    #[error(transparent)]
    RepoUrlParse(#[from] RepoUrlParseError),
}

/// Error produced when a repository URL fails validation.
///
/// Repository URLs parsed from testreport metadata are interpolated into remote
/// `zypper ar`/`rr` commands executed as root; a value carrying shell
/// metacharacters, whitespace, an option-like leading dash, or an unsupported
/// URI scheme is a command-injection vector. [`RepoUrl`](crate::repo_url::RepoUrl)
/// rejects such input with a typed error *before* it reaches host execution.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RepoUrlParseError {
    /// The URL was empty.
    #[error("repo url: empty")]
    Empty,

    /// The URL began with `-`, so it would be read as a command option.
    #[error("repo url: option-like (leading '-'): {url:?}")]
    OptionLike {
        /// The offending URL.
        url: String,
    },

    /// The URL had no `scheme://` prefix.
    #[error("repo url: missing scheme: {url:?}")]
    MissingScheme {
        /// The offending URL.
        url: String,
    },

    /// The URL's scheme is not one zypper/libzypp accepts for a repository.
    #[error("repo url: unsupported scheme {scheme:?}")]
    UnsupportedScheme {
        /// The offending scheme.
        scheme: String,
    },

    /// The URL contained a shell-unsafe or control character.
    #[error("repo url: illegal character {ch:?} in url: {url:?}")]
    IllegalChar {
        /// The disallowed character.
        ch: char,
        /// The offending URL.
        url: String,
    },
}

/// Error produced when a package name or `name=version` spec fails validation.
///
/// Package specifiers parsed from testreport metadata are interpolated into
/// remote commands executed as root; a value carrying shell metacharacters,
/// whitespace, or an option-like leading dash is a command-injection vector.
/// [`PackageSpec`](crate::package_spec::PackageSpec) rejects such input with a
/// typed error *before* it reaches host execution.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackageSpecParseError {
    /// The name (or the name half of a `name=version` spec) was empty.
    #[error("package spec: empty name")]
    Empty,

    /// The name began with `-`, so it would be read as a command option.
    #[error("package spec: option-like name (leading '-'): {name:?}")]
    OptionLike {
        /// The offending name.
        name: String,
    },

    /// The name contained a character outside the RPM name allow-list
    /// (`[A-Za-z0-9._+-]`).
    #[error("package spec: illegal character {ch:?} in name: {name:?}")]
    IllegalChar {
        /// The disallowed character.
        ch: char,
        /// The offending name.
        name: String,
    },

    /// The version half of a `name=version` spec was empty or contained a
    /// character outside the version allow-list (`[A-Za-z0-9.:_+~^-]`).
    #[error("package spec: invalid version {version:?} in spec: {spec:?}")]
    BadVersion {
        /// The offending version half.
        version: String,
        /// The full `name=version` spec that failed.
        spec: String,
    },
}

/// Error produced when a `refhosts.yml` document cannot be parsed.
///
/// A document-level YAML parse failure propagates as this typed error wrapping
/// the underlying `serde-saphyr` failure. Note: individual *malformed rows* do not
/// surface here — they are dropped (logged) so one bad row
/// never aborts the whole load. Only a document-level YAML failure is fatal.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RefhostsParseError {
    /// The YAML document itself was malformed.
    #[error("failed to parse refhosts.yml: {0}")]
    Yaml(#[from] serde_saphyr::Error),
}

/// Error produced when an RPM version string cannot be parsed.
///
/// An empty version string is a typed, fallible parse error rather than a
/// panic, following the crate's fallible-constructor convention (see
/// [`RridParseError`]).
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpmVersionParseError {
    /// The version string was empty.
    #[error("RPM version: empty version string")]
    Empty,
}

/// Error produced when a request-kind token is not recognised.
///
/// The `{raw:?}` debug formatting renders the offending token quoted.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("unknown request kind: {raw:?}")]
pub struct RequestKindParseError {
    /// The raw token that failed to parse.
    pub(crate) raw: String,
}

/// Errors produced while parsing an OBS Request Review ID (RRID).
///
/// Every message is rendered with the `"OBS Request Review ID: "` prefix so a
/// parse failure is self-identifying in a log line or a CI transcript, where the
/// bare component error would be ambiguous.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RridParseError {
    /// The RRID had more `:`-separated components than allowed.
    #[error("OBS Request Review ID: Too many components (> {limit})")]
    TooManyComponents {
        /// The maximum number of components allowed.
        limit: usize,
    },

    /// A required component was absent.
    #[error("OBS Request Review ID: Missing {index}. component. Expected: {expected}")]
    MissingComponent {
        /// 1-based index of the missing component.
        index: usize,
        /// Human-readable description of what was expected.
        expected: String,
    },

    /// A component was present but could not be parsed.
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
    fn too_many_components_message_is_stable() {
        let err = RridParseError::TooManyComponents { limit: 4 };
        assert_eq!(
            err.to_string(),
            "OBS Request Review ID: Too many components (> 4)"
        );
    }

    #[test]
    fn missing_component_message_is_stable() {
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
    fn component_parse_message_is_stable() {
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
    fn internal_message_is_stable() {
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
