//! The `mtui-datasources` error hierarchy.
//!
//! [`HttpError`] wraps the shared HTTP policy layer; each client (openQA, QEM
//! dashboard, Gitea, oqa-search) has its own `#[from]` sub-error enum.

use mtui_types::Assignment;
use thiserror::Error;

/// Convenience alias for `Result<T, `[`enum@HttpError`]`>`.
pub type Result<T> = std::result::Result<T, HttpError>;

/// Errors from the shared outbound HTTP layer.
///
/// Transport failures and non-2xx statuses collapse onto the underlying
/// [`reqwest::Error`]; [`CaBundle`](Self::CaBundle) is separate because
/// reqwest's rustls backend needs a user-configured CA bundle read from disk
/// eagerly at client-build time.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpError {
    /// A transport failure, a non-2xx HTTP status, or a client-build failure
    /// surfaced by `reqwest`.
    ///
    /// Renders as reqwest's kind plus the deepest cause (`error sending
    /// request: Connection refused (os error 111)`): reqwest's own `Display`
    /// stops at the kind, which made a reset, a DNS failure, a TLS failure and
    /// both timeouts read identically (#594).
    ///
    /// **Invariant: the wrapped error carries no URL**, stripped by
    /// [`From<reqwest::Error>`](HttpError#impl-From<Error>-for-HttpError), and
    /// the cause comes through `root_cause`, i.e. through `sanitize_url`, so
    /// neither half can render one.
    /// `#[non_exhaustive]` makes the conversion the only way to build the
    /// variant from outside this crate (`E0639`); inside it is convention, so
    /// construct via `?`/`.into()` and never bare. Matching is unaffected.
    #[error("{}", request_display(.0))]
    #[non_exhaustive]
    Request(#[source] reqwest::Error),

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
    /// A defence against a hostile/misconfigured datasource OOMing mtui with an
    /// arbitrarily large, `Content-Length`-lying or endless chunked body. The
    /// message deliberately carries no URL, so it can never leak credentials
    /// embedded in a datasource URL.
    #[error("response body exceeds the {limit}-byte limit")]
    BodyTooLarge {
        /// The maximum number of bytes the caller was willing to buffer.
        limit: usize,
        /// The advertised `Content-Length` if the body was rejected early,
        /// else `None` (the cap tripped while streaming).
        seen: Option<u64>,
    },
}

impl HttpError {
    /// The HTTP status code that caused this failure, when the failure was a
    /// non-2xx response rather than a transport-level failure (`Some` only for
    /// `Request` where the wrapped `reqwest::Error` carries one — a
    /// `CaBundle`/`BodyTooLarge` failure never reached the wire).
    ///
    /// Status codes are not credentials, so returning one is not a
    /// URL-sanitization concern the way the wrapped error's `Display` is.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Request(e) => e.status().map(|s| s.as_u16()),
            Self::CaBundle { .. } | Self::BodyTooLarge { .. } => None,
        }
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(e: reqwest::Error) -> Self {
        // #431: reqwest's `Display` appends the request URL verbatim
        // (" for url (…)"), and `HttpError::Request` renders it, as does every
        // `…Error::Http` over it, so it surfaces wherever an error is rendered
        // — and it is not reliably credential-free: `Response::error_for_status`
        // reports the *redirect-updated* URL, so a `Location` header can put
        // `user:pass@host` straight back. Supplying sanitized request context
        // is the call site's job, via `crate::http::sanitize_url`.
        Self::Request(e.without_url())
    }
}

/// `"{kind}: {deepest cause}"`, or the kind alone when reqwest attached no
/// source.
fn request_display(e: &reqwest::Error) -> String {
    let phase = timeout_phase(e.is_timeout(), e.is_connect());
    match crate::http::root_cause(e) {
        Some(cause) => format!("{e}: {cause}{phase}"),
        None => format!("{e}{phase}"),
    }
}

/// reqwest renders the 5 s connect and 30 s read timeouts identically
/// ("operation timed out"); the phase says which budget was hit.
fn timeout_phase(is_timeout: bool, is_connect: bool) -> &'static str {
    match (is_timeout, is_connect) {
        (false, _) => "",
        (true, true) => " (connect timeout)",
        (true, false) => " (read timeout)",
    }
}

/// Errors from loading and parsing a local `refhosts.yml` database.
///
/// Per-row malformation never reaches this hierarchy: it is dropped + logged
/// lower down by [`mtui_types::load_refhosts`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RefhostError {
    /// A `refhosts.yml` I/O operation (read, stat, or mirror write) failed.
    #[error("refhosts.yml I/O error at {path}: {source}")]
    Io {
        /// The path the failing operation targeted.
        path: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },

    /// The `refhosts.yml` contents are not a valid document.
    #[error(transparent)]
    Parse(#[from] mtui_types::RefhostsParseError),

    /// No configured resolver could produce a usable `refhosts.yml`.
    ///
    /// The terminal "all strategies exhausted" signal from
    /// [`RefhostsFactory`](crate::refhost::RefhostsFactory); the individual
    /// resolver failures are logged at `warn` as they happen.
    #[error("no refhosts resolver could produce a usable database")]
    ResolveFailed,

    /// A concurrent HTTPS cache refresh failed while this resolve waited on it.
    ///
    /// The waiter does not retry: serialised behind the refresh lock, each retry
    /// costs a full transport timeout, multiplying a down server's latency by
    /// the number of waiters. Failing fast caps that at one timeout, and the
    /// caller falls back to the next configured resolver exactly as it would for
    /// the leader's own error.
    #[error("refhosts refresh skipped: a concurrent download attempt just failed")]
    RefreshJustFailed,
}

/// Errors from building the openQA API client or fetching jobs.
///
/// [`OpenQABase::get_jobs`](crate::openqa) is best-effort (fetch failures fold
/// to a "no jobs" [`None`]); [`try_get_jobs`](crate::openqa) surfaces them as
/// [`Fetch`](Self::Fetch) so a caller can tell a genuinely-empty result apart
/// from an unreachable openQA. `ruoqa::Error` is `#[non_exhaustive]` with eleven
/// failure modes; all collapse onto
/// [`Fetch`](Self::Fetch)/[`ClientBuild`](Self::ClientBuild) via the
/// `openqa::client` module's `describe` helper. Userinfo cannot reach these
/// messages: `ruoqa` >= 0.1.4 redacts it at every `Display` site and drops it in
/// `config::resolve`, and `describe` never renders a `reqwest::Error` directly.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenQAError {
    /// The underlying HTTP layer failed to build the injected transport (e.g.
    /// an unreadable CA bundle).
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The `ruoqa` client itself failed to build: a malformed `client.conf`,
    /// invalid TLS setup, or an incompatible builder combination. Userinfo
    /// cannot reach this message (see the module-level doc above).
    #[error("openQA client could not be built: {0}")]
    ClientBuild(String),

    /// A jobs fetch failed: a transport error, a non-2xx status, or a malformed
    /// response body. Userinfo cannot reach this message (see the
    /// module-level doc above).
    #[error("openQA jobs fetch failed: {0}")]
    Fetch(String),
}

/// Errors from the Gitea PR review-workflow connector ([`crate::gitea`]).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GiteaError {
    /// The Gitea API token is empty, so the client cannot authenticate.
    #[error("Gitea API token is empty, can't access API")]
    MissingToken,

    /// An API call failed (transport error or non-2xx status). The payload is
    /// a `"{method} - {url}"` (optionally with the status) context.
    #[error("Gitea API call failed: {0}")]
    FailedCall(String),

    /// The PR has no pending review for the group, or was already decided.
    #[error("{0}")]
    NoReview(String),

    /// The PR is not in the assignment state the operation requires.
    #[error("{}", assign_invalid_message(*state, user))]
    AssignInvalid {
        /// The current assignment state that made the operation invalid.
        state: Assignment,
        /// The user the operation was attempted on behalf of.
        user: String,
    },

    /// A URL passed for PR-API conversion is not a recognisable Gitea PR URL.
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
/// Slack's Web API reports application-level failures as **HTTP 200** with
/// `{"ok": false, "error": "&lt;code&gt;"}`, so a 2xx status proves nothing —
/// [`Api`](Self::Api) carries that code. It also rate-limits with `429` plus a
/// `Retry-After` header, which a watch loop must treat as "back off and keep
/// going" rather than as a failure — hence the dedicated
/// [`RateLimited`](Self::RateLimited) variant.
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
    /// payload is a `"{method} - {url}"` context, always sanitized — never
    /// the token.
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
/// The high-level entry points (`single_incidents`, `aggregated_updates`,
/// `build_checks`) fold it into a typed note / empty result, so it escapes only
/// from the lower-level `get_incident_info` and `incident_jobs`, whose callers
/// convert it into a user-facing message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OqaSearchError {
    /// A transport failure, a non-2xx HTTP status, or a malformed JSON body
    /// from an openQA / Dashboard / QAM endpoint.
    #[error("openQA/Dashboard request failed: {0}")]
    Http(String),
}

impl From<HttpError> for OqaSearchError {
    fn from(source: HttpError) -> Self {
        Self::Http(source.to_string())
    }
}

impl From<ruoqa::Error> for OqaSearchError {
    /// Routed through `openqa::client::describe` first: a fetch failure must
    /// never carry a raw URL, which could embed a credentialed openQA instance
    /// URL (see [`OpenQAError`]).
    fn from(source: ruoqa::Error) -> Self {
        Self::Http(crate::openqa::client::describe(&source))
    }
}

/// Errors from the QEM Dashboard connector ([`crate::qem_dashboard`]).
///
/// The default read helpers are best-effort: a fetch failure is logged at
/// `debug` and folded into a `None`/empty result. The `try_*` variants surface
/// it as [`Fetch`](Self::Fetch), letting
/// [`DashboardAutoOpenQA::run`](crate::qem_dashboard::DashboardAutoOpenQA)
/// distinguish an unreachable dashboard from a genuinely-empty result.
/// [`Http`](Self::Http) covers the failure that surfaces *before* any request:
/// building the shared [`HttpClient`](crate::http::HttpClient).
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
/// Reads are best-effort by default: a fetch failure folds to `None` so a
/// hiccup never aborts a command. The `try_*` reads surface it as
/// [`Fetch`](Self::Fetch), so a caller can tell an empty successful response
/// apart from a transport/status/JSON failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TeReGenError {
    /// A TeReGen fetch failed: a transport error, a non-2xx status, or a
    /// malformed JSON body. Carries a sanitized description (never the raw URL).
    #[error("TeReGen fetch failed: {0}")]
    Fetch(String),
}

/// Render the trailing `" (retry after Ns)"` clause of
/// [`SlackError::RateLimited`], omitted when Slack sent no `Retry-After`.
fn retry_after_suffix(retry_after: Option<u64>) -> String {
    match retry_after {
        Some(secs) => format!(" (retry after {secs}s)"),
        None => String::new(),
    }
}

/// Render the [`GiteaError::AssignInvalid`] message for an assignment state.
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
    fn assign_invalid_display_messages_are_stable() {
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

    /// The #431 boundary: a `reqwest::Error` entering [`HttpError`] loses its
    /// URL. Driven by a real transport error (nothing listens on the loopback
    /// discard port, RFC 863) because the URL is attached by reqwest's own send
    /// path, not by anything constructible here.
    #[tokio::test]
    async fn reqwest_error_loses_its_url_at_the_http_error_boundary() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:9/x")
            .send()
            .await
            .expect_err("nothing listens on the discard port");
        // Premise guard: without an appended URL the boundary below is untested.
        assert!(
            e.to_string().contains(" for url ("),
            "premise gone: reqwest no longer appends the URL: {e}"
        );

        let msg = HttpError::from(e).to_string();
        assert!(
            !msg.contains(" for url ("),
            "URL crossed the boundary: {msg}"
        );
        // The error *kind* must survive the strip — only the URL is dropped.
        assert!(
            msg.contains("error sending request"),
            "kind was lost with the URL: {msg}"
        );
    }

    /// #594: reqwest's `Display` stops at the kind, so without the cause a
    /// refused connection, a reset, a DNS failure and a timeout all read
    /// `error sending request`. The variant must render the deepest cause
    /// after the kind, and still without the URL.
    #[tokio::test]
    async fn request_display_carries_the_transport_cause() {
        let e = reqwest::Client::new()
            .get("http://127.0.0.1:9/x")
            .send()
            .await
            .expect_err("nothing listens on the discard port");
        assert!(
            std::error::Error::source(&e).is_some(),
            "premise gone: reqwest attached no source to a refused connect"
        );

        let msg = HttpError::from(e).to_string();
        assert!(
            msg.starts_with("error sending request: "),
            "cause is not appended to the kind: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("connection refused"),
            "cause was lost: {msg}"
        );
        assert!(!msg.contains(" for url ("), "URL leaked: {msg}");
    }

    /// The custom `Display` must not cost the `source()` chain: the TLS hint
    /// (`is_ssl_verification_error`) and `ssl_error_detail` walk it.
    #[tokio::test]
    async fn request_source_chain_is_intact_under_the_custom_display() {
        let he: HttpError = reqwest::Client::new()
            .get("http://127.0.0.1:9/x")
            .send()
            .await
            .expect_err("nothing listens on the discard port")
            .into();
        assert!(
            std::error::Error::source(&he).is_some(),
            "source() chain broken by the Display change"
        );
        let cause = crate::http::root_cause(&he).expect("chain reaches a cause");
        assert!(
            cause.to_lowercase().contains("connection refused"),
            "walk stopped short of the transport cause: {cause}"
        );
    }

    /// Both reqwest timeouts render as `operation timed out`; a read timeout
    /// (the server accepted the connection and stalled) must say so.
    #[tokio::test]
    async fn read_timeout_is_named_as_such() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        // A test-local client: the property under test is the Display, and
        // `HttpClient`'s 30 s read timeout cannot be driven offline.
        let e = reqwest::Client::builder()
            .read_timeout(std::time::Duration::from_millis(20))
            .build()
            .expect("client builds")
            .get(format!("{}/slow", server.uri()))
            .send()
            .await
            .expect_err("the body stalls past the read timeout");
        assert!(
            e.is_timeout() && !e.is_connect(),
            "premise gone: not a read timeout: {e:?}"
        );

        let msg = HttpError::from(e).to_string();
        assert!(
            msg.contains("operation timed out"),
            "timeout cause was lost: {msg}"
        );
        assert!(
            msg.ends_with(" (read timeout)"),
            "read-timeout phase missing: {msg}"
        );
    }

    /// The connect arm cannot be driven offline deterministically (a
    /// black-holed connect is network-dependent), so the table is pinned on
    /// the pure core.
    #[test]
    fn timeout_phase_truth_table() {
        assert_eq!(timeout_phase(false, false), "");
        assert_eq!(timeout_phase(false, true), "");
        assert_eq!(timeout_phase(true, true), " (connect timeout)");
        assert_eq!(timeout_phase(true, false), " (read timeout)");
    }
}
