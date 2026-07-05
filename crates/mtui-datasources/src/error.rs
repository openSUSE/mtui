//! The `mtui-datasources` error hierarchy.
//!
//! This crate holds every outbound integration (the shared HTTP policy layer,
//! the `refhosts.yml` resolver/search/verify, and the external service
//! clients). The first landed surface is the HTTP layer ported from upstream
//! `mtui/support/http.py`, so [`HttpError`] is the first member of the
//! hierarchy. Later Phase-3 tasks add their own `#[from]` sub-errors (openQA,
//! QEM dashboard, Gitea, oqa-search) as those clients land, so each variant is
//! exercised by real tests rather than sitting dead.

use mtui_types::Assignment;
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

/// Errors from the Gitea PR review-workflow connector ([`crate::gitea`]).
///
/// Mirrors the `GiteaError` exception family in upstream
/// `mtui/support/exceptions.py`. Each variant maps to one upstream exception:
///
/// * [`MissingToken`](Self::MissingToken) → `MissingGiteaTokenError`;
/// * [`FailedCall`](Self::FailedCall) → `FailedGiteaCallError` (any transport
///   failure or non-2xx status from the API);
/// * [`NoReview`](Self::NoReview) → `GiteaNoReviewError` (no review requested,
///   or the PR was already approved/rejected);
/// * [`AssignInvalid`](Self::AssignInvalid) → `GiteaAssignInvalidError`, whose
///   message is chosen by the [`Assignment`] state exactly as upstream's
///   `__str__` does;
/// * [`InvalidPrUrl`](Self::InvalidPrUrl) → the `ValueError` raised by
///   `pr_api_url` for a non-PR URL.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GiteaError {
    /// The Gitea API token is empty, so the client cannot authenticate.
    #[error("Gitea API token is empty, can't access API")]
    MissingToken,

    /// An API call failed (transport error or non-2xx status). The payload is
    /// the upstream `"{method} - {url}"` (optionally with the status) context.
    #[error("Gitea API call failed: {0}")]
    FailedCall(String),

    /// The PR has no pending review for the group, or was already decided.
    #[error("{0}")]
    NoReview(String),

    /// The PR is not in the assignment state the operation requires. The
    /// message reproduces upstream `GiteaAssignInvalidError.__str__`.
    #[error("{}", assign_invalid_message(*state, user))]
    AssignInvalid {
        /// The current assignment state that made the operation invalid.
        state: Assignment,
        /// The user the operation was attempted on behalf of.
        user: String,
    },

    /// A URL passed to [`pr_api_url`](crate::gitea::pr_api_url) is not a
    /// recognisable Gitea PR URL.
    #[error("not a Gitea PR URL: {0}")]
    InvalidPrUrl(String),

    /// The underlying HTTP layer failed to build the request or client.
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Errors from the `osc qam` subprocess wrapper ([`crate::oscqam`]).
///
/// Upstream `mtui/data_sources/oscqam.py` is best-effort: every failure is
/// logged and folded into a `bool` return. This port deviates intentionally to
/// an idiomatic typed `Result<(), OscError>` (see the crate AGENTS notes on
/// preferring a typed `Result` over MTUI's `log + return`), preserving the
/// *behaviour* — the operation still never panics and each failure carries the
/// reason `osc` gave — while making that reason inspectable by the caller. The
/// variants map onto upstream's three logged failure paths plus the runner
/// seam:
///
/// * [`NonZero`](Self::NonZero) → upstream `CalledProcessError` (osc exited
///   non-zero); the payload is `_tail`-trimmed stderr/stdout or the bare exit
///   code, exactly as upstream logged it.
/// * [`Timeout`](Self::Timeout) → upstream `TimeoutExpired` (osc did not return
///   within the runtime cap, likely an interactive prompt with no input).
/// * [`NotFound`](Self::NotFound) → upstream `FileNotFoundError` (`osc` is not
///   installed or not on `PATH`).
/// * [`Runner`](Self::Runner) → any other I/O failure spawning/awaiting the
///   child process (Rust-specific: upstream `run` folded these into the same
///   exception paths).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OscError {
    /// `osc` exited with a non-zero status. The payload reproduces upstream's
    /// logged detail: the trimmed stderr, or trimmed stdout, or `exit code N`.
    #[error("osc '{operation}' operation failed: {detail}")]
    NonZero {
        /// The `qam` subcommand that failed (e.g. `approve`).
        operation: String,
        /// The trimmed osc stderr/stdout, or `exit code N` when both were empty.
        detail: String,
    },

    /// `osc` did not return within the runtime cap (upstream 180s), likely an
    /// interactive prompt with no input on the detached stdin.
    #[error(
        "osc '{operation}' operation timed out after {seconds}s; osc did not return \
         (likely an interactive prompt with no input)"
    )]
    Timeout {
        /// The `qam` subcommand that timed out.
        operation: String,
        /// The elapsed runtime cap in seconds.
        seconds: u64,
    },

    /// The `osc` binary could not be found; it is not installed or not on
    /// `PATH`.
    #[error("'osc' command not found. Is it installed and in your PATH?")]
    NotFound,

    /// Any other I/O failure spawning or awaiting the `osc` child process.
    #[error("failed to run osc '{operation}': {source}")]
    Runner {
        /// The `qam` subcommand being run when the failure occurred.
        operation: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
}

/// Errors from the openQA / QAM Dashboard overview search ([`crate::oqa_search`]).
///
/// Mirrors upstream's single `_HTTPError` (raised by `_get_json` /
/// `_fetch_url_content` on any transport or non-2xx / bad-JSON failure). The
/// three high-level entry points (`single_incidents`, `aggregated_updates`,
/// `build_checks`) catch it internally and fold it into a typed note / empty
/// result, exactly as upstream does, so it never escapes them. It surfaces only
/// from the lower-level fetch helpers that upstream also lets propagate —
/// `get_incident_info` and `incident_jobs` — where the caller is expected to
/// convert it into a user-facing message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OqaSearchError {
    /// A transport failure, a non-2xx HTTP status, or a malformed JSON body
    /// from an openQA / Dashboard / QAM endpoint. Corresponds to upstream
    /// `_HTTPError`.
    #[error("openQA/Dashboard request failed: {0}")]
    Http(String),
}

impl From<HttpError> for OqaSearchError {
    fn from(source: HttpError) -> Self {
        Self::Http(source.to_string())
    }
}

/// Errors from the QEM Dashboard connector ([`crate::qem_dashboard`]).
///
/// The dashboard client is read-only and best-effort: like upstream
/// `QEMDashboardClient._get`, every *fetch* failure (transport, non-2xx, bad
/// JSON) is logged at `debug` and folded into a `None`/empty result, so a fetch
/// error never escapes the client. This error type therefore covers only the
/// failure that surfaces *before* any request — building the shared
/// [`HttpClient`](crate::http::HttpClient) (e.g. an unreadable CA bundle) — via
/// the `#[from] HttpError` conversion used by `QemDashboardClient::new` and
/// `QemIncident::new`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum QemDashboardError {
    /// The underlying HTTP layer failed to build the client (e.g. a
    /// user-configured CA bundle could not be read or parsed).
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Render the [`GiteaError::AssignInvalid`] message for an assignment state,
/// mirroring upstream `GiteaAssignInvalidError.__str__` verbatim.
fn assign_invalid_message(state: Assignment, user: &str) -> String {
    match state {
        Assignment::AssignedOther => format!("Gitea PR has assigned different user than {user}"),
        Assignment::AssignedUser => format!("Gitea PR has already assigned user: {user}"),
        Assignment::Unassigned => format!("User {user} isnt assigned to Gitea PR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_invalid_display_matches_upstream_messages() {
        // Reproduces GiteaAssignInvalidError.__str__ for each assignment state.
        let other = GiteaError::AssignInvalid {
            state: Assignment::AssignedOther,
            user: "alice".to_string(),
        };
        assert_eq!(
            other.to_string(),
            "Gitea PR has assigned different user than alice"
        );

        let already = GiteaError::AssignInvalid {
            state: Assignment::AssignedUser,
            user: "alice".to_string(),
        };
        assert_eq!(
            already.to_string(),
            "Gitea PR has already assigned user: alice"
        );

        let none = GiteaError::AssignInvalid {
            state: Assignment::Unassigned,
            user: "alice".to_string(),
        };
        assert_eq!(none.to_string(), "User alice isnt assigned to Gitea PR");
    }

    #[test]
    fn gitea_error_display_variants() {
        assert_eq!(
            GiteaError::MissingToken.to_string(),
            "Gitea API token is empty, can't access API"
        );
        assert!(
            GiteaError::InvalidPrUrl("x".to_string())
                .to_string()
                .contains("not a Gitea PR URL")
        );
        assert!(
            GiteaError::FailedCall("GET - /x".to_string())
                .to_string()
                .contains("GET - /x")
        );
    }
}
