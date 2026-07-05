//! The `mtui-datasources` error hierarchy.
//!
//! This crate holds every outbound integration (the shared HTTP policy layer,
//! the `refhosts.yml` resolver/search/verify, and the external service
//! clients). The first landed surface is the HTTP layer ported from upstream
//! `mtui/support/http.py`, so [`HttpError`] is the first member of the
//! hierarchy. Later Phase-3 tasks add their own `#[from]` sub-errors (openQA,
//! QEM dashboard, Gitea, oqa-search) as those clients land, so each variant is
//! exercised by real tests rather than sitting dead.

use thiserror::Error;

/// Convenience alias for `Result<T, `[`enum@HttpError`]`>`.
pub type Result<T> = std::result::Result<T, HttpError>;

/// Errors from the shared outbound HTTP layer.
///
/// Mirrors the failure surface upstream `get_bytes` exposes: any transport
/// failure or non-2xx status propagates as a `requests.exceptions.*`. Here that
/// collapses onto the underlying [`reqwest::Error`], but a dedicated
/// [`CaBundle`](Self::CaBundle) variant is added for the Rust-specific step of
/// reading a user-configured CA bundle from disk (upstream handed the path
/// straight to `requests`; reqwest's rustls backend needs the PEM loaded
/// eagerly at client-build time).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpError {
    /// A transport failure, a non-2xx HTTP status, or a client-build failure
    /// surfaced by `reqwest`.
    #[error(transparent)]
    Request(#[from] reqwest::Error),

    /// A user-configured CA bundle could not be read or parsed into
    /// certificates when building the HTTP client.
    #[error("failed to load CA bundle {path}: {source}")]
    CaBundle {
        /// The CA bundle path from the `ssl_verify` config.
        path: String,
        /// The underlying I/O or certificate-parse failure.
        source: std::io::Error,
    },
}

/// Errors from loading and parsing a local `refhosts.yml` database.
///
/// Mirrors upstream `Refhosts._parse_refhosts`, which logs at ERROR and
/// re-raises: a file that cannot be read surfaces as [`Io`](Self::Io) and a
/// document-level YAML failure as [`Parse`](Self::Parse). Per-row malformation
/// is handled lower down (dropped + logged by
/// [`mtui_types::load_refhosts`]), so it never reaches this hierarchy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RefhostError {
    /// The `refhosts.yml` file could not be read from disk.
    #[error("failed to read refhosts.yml {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },

    /// The `refhosts.yml` contents are not a valid document.
    #[error(transparent)]
    Parse(#[from] mtui_types::RefhostsParseError),

    /// No configured resolver could produce a usable `refhosts.yml`.
    ///
    /// Mirrors upstream `RefhostsResolveFailedError`: the
    /// [`RefhostsFactory`](crate::refhost::RefhostsFactory) tried every resolver
    /// named in `config.refhosts_resolvers` (in order) and each one either was
    /// unknown or failed. The individual failures are logged at `warn` as they
    /// happen; this variant is the terminal "all strategies exhausted" signal.
    #[error("no refhosts resolver could produce a usable database")]
    ResolveFailed,
}

/// Errors from building an openQA API request.
///
/// The openQA connectors ([`crate::openqa`]) fold all *fetch* failures into a
/// "no jobs" [`None`] result (mirroring upstream, where any transport error is
/// logged and turned into `None` so a command never aborts on a flaky openQA).
/// This error type therefore covers only the failures that surface *before* the
/// request is dispatched — building the signed request — plus the HMAC/clock
/// preconditions that must hold for signing.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenQAError {
    /// The underlying HTTP layer failed to build the request or client.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The system clock is before the Unix epoch, so a request microtime
    /// cannot be computed (required for the `X-API-Microtime` auth header).
    #[error("system clock is before the Unix epoch; cannot compute request microtime")]
    Clock,
}
