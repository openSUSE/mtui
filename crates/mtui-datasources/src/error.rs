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

    /// The response body exceeded the endpoint's maximum allowed size.
    ///
    /// A defence against a hostile/misconfigured datasource returning an
    /// arbitrarily large (or `Content-Length`-lying, or endless chunked) body
    /// that would OOM/DoS mtui. `seen` carries the advertised `Content-Length`
    /// when the body was rejected *before* any read; it is `None` when the cap
    /// tripped mid-stream (unknown/lying length). The message deliberately
    /// carries no URL, so it can never leak credentials embedded in a
    /// datasource URL.
    #[error("response body exceeds the {limit}-byte limit")]
    BodyTooLarge {
        /// The maximum number of bytes the caller was willing to buffer.
        limit: usize,
        /// The advertised `Content-Length` if the body was rejected early,
        /// else `None` (the cap tripped while streaming).
        seen: Option<u64>,
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

/// Errors from building an openQA API request or fetching jobs.
///
/// The connectors' best-effort helper [`OpenQABase::get_jobs`](crate::openqa)
/// still folds all *fetch* failures into a "no jobs" [`None`] result (mirroring
/// upstream). The fallible variant
/// [`OpenQABase::try_get_jobs`](crate::openqa) instead surfaces a fetch failure
/// as [`Fetch`](Self::Fetch) so a caller (e.g. `KernelOpenQA::run`) can tell a
/// genuinely-empty result apart from an unreachable openQA. This type also
/// covers the failures that surface *before* the request is dispatched —
/// building the signed request — plus the HMAC/clock preconditions for signing.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenQAError {
    /// The underlying HTTP layer failed to build the request or client.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// A jobs fetch failed: a transport error, a non-2xx status, or a malformed
    /// JSON body. Carries a sanitized description (never the raw URL).
    #[error("openQA jobs fetch failed: {0}")]
    Fetch(String),

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

    /// The metadata-supplied Gitea URL is not the configured trusted origin (or
    /// is not `https`, or carries userinfo), so the token was **not** sent. The
    /// payload is the sanitised URL — never the token or any credential. Set
    /// `[gitea] url` (`config set gitea_url`) to the trusted Gitea origin.
    #[error(
        "refusing to send Gitea token to untrusted URL {0}: it must be an https \
         URL whose origin matches the configured trusted Gitea origin \
         ([gitea] url / `config set gitea_url`)"
    )]
    UntrustedOrigin(String),

    /// The underlying HTTP layer failed to build the request or client.
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Errors from the Slack review-request connector ([`crate::slack`]).
///
/// Slack's Web API is unusual in two ways this enum has to model explicitly.
/// It reports application-level failures as **HTTP 200** with `{"ok": false,
/// "error": "&lt;code&gt;"}`, so a successful status tells you nothing —
/// [`Api`](Self::Api) carries that code. And it rate-limits with `429` plus a
/// `Retry-After` header, which callers must treat as "back off and keep going"
/// rather than as a failure — hence the dedicated
/// [`RateLimited`](Self::RateLimited) variant, so a watch loop can distinguish
/// throttling from a genuine error instead of counting it as one.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SlackError {
    /// The Slack bot token is empty, so the client cannot authenticate.
    #[error(
        "Slack API token is empty; set it with `config set slack_token &lt;token&gt;` \
         or the `[slack] token` config key"
    )]
    MissingToken,

    /// No Slack channel is configured to post the review request to.
    #[error(
        "no Slack channel configured; set it with `config set slack_channel &lt;channel&gt;` \
         or the `[slack] channel` config key"
    )]
    MissingChannel,

    /// The Slack integration is switched off in the configuration.
    #[error("Slack integration is disabled ([slack] enabled = false)")]
    Disabled,

    /// A call failed at the transport level or returned a non-2xx status. The
    /// payload is the upstream-style `"{method} - {url}"` context, always
    /// sanitized — never the token.
    #[error("Slack API call failed: {0}")]
    FailedCall(String),

    /// The call reached Slack but was refused at the application level
    /// (HTTP 200 with `ok: false`). The payload is Slack's own error code,
    /// such as `channel_not_found`, `not_in_channel` or `invalid_auth`.
    #[error("Slack API returned an error: {0}")]
    Api(String),

    /// Slack rate-limited the call (`429 Too Many Requests`). A watch loop
    /// treats this as "still watching" and backs off; it is not a failure.
    #[error("Slack API rate limited{}", retry_after_suffix(*retry_after))]
    RateLimited {
        /// The `Retry-After` header in seconds, when the server sent one.
        retry_after: Option<u64>,
    },

    /// The configured API base is not the trusted Slack origin (or is not
    /// `https`, or carries userinfo), so the token was **not** sent. The
    /// payload is the sanitized URL — never the token.
    #[error(
        "refusing to send Slack token to untrusted URL {0}: it must be an https \
         URL whose origin matches the configured Slack API base \
         ([slack] api_url / `config set slack_api_url`)"
    )]
    UntrustedOrigin(String),

    /// The underlying HTTP layer failed to build the request or client.
    #[error(transparent)]
    Http(#[from] HttpError),
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
/// The dashboard client's default read helpers remain best-effort: like upstream
/// `QEMDashboardClient._get`, every *fetch* failure (transport, non-2xx, bad
/// JSON) is logged at `debug` and folded into a `None`/empty result, so a fetch
/// error never escapes them. The fallible `try_*` variants instead surface a
/// fetch failure as [`Fetch`](Self::Fetch), letting
/// [`DashboardAutoOpenQA::run`](crate::qem_dashboard::DashboardAutoOpenQA)
/// distinguish an unreachable dashboard from a genuinely-empty result. The
/// [`Http`](Self::Http) variant still covers the failure that surfaces *before*
/// any request — building the shared [`HttpClient`](crate::http::HttpClient)
/// (e.g. an unreadable CA bundle) — via the `#[from] HttpError` conversion used
/// by `QemDashboardClient::new` and `QemIncident::new`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum QemDashboardError {
    /// The underlying HTTP layer failed to build the client (e.g. a
    /// user-configured CA bundle could not be read or parsed).
    #[error(transparent)]
    Http(#[from] HttpError),

    /// A dashboard fetch failed: a transport error, a non-2xx status, or a
    /// malformed JSON body. Carries a sanitized description (never the raw URL).
    #[error("QEM Dashboard fetch failed: {0}")]
    Fetch(String),
}

/// Errors from the TeReGen Report API client ([`crate::teregen`]).
///
/// TeReGen reads are best-effort by default (like upstream `_get`): every fetch
/// failure folds to `None` so a hiccup never aborts a command. The fallible
/// `try_*` reads instead surface a fetch failure as [`Fetch`](Self::Fetch), so a
/// caller can distinguish a genuinely-empty successful response from a
/// transport/status/JSON failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TeReGenError {
    /// A TeReGen fetch failed: a transport error, a non-2xx status, or a
    /// malformed JSON body. Carries a sanitized description (never the raw URL).
    #[error("TeReGen fetch failed: {0}")]
    Fetch(String),
}

/// Render the trailing `" (retry after Ns)"` clause of
/// [`SlackError::RateLimited`], omitted entirely when Slack sent no
/// `Retry-After` header.
fn retry_after_suffix(retry_after: Option<u64>) -> String {
    match retry_after {
        Some(secs) => format!(" (retry after {secs}s)"),
        None => String::new(),
    }
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
