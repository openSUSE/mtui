//! Builds the [`ruoqa::Client`] used by the openQA connectors.
//!
//! `ruoqa` owns the whole signed-request contract (INI `client.conf` /
//! `$OPENQA_CONFIG` discovery, HMAC-SHA1 `X-API-Hash`, retries, and
//! same-origin-only redirects) that this module used to hand-roll. The only
//! thing left here is wiring mtui's shared TLS/timeout policy into it: an
//! injected, redirect-less, no-reqwest-retry `reqwest::Client` (see
//! [`crate::http::HttpClient::openqa_transport`]), because `ruoqa` re-signs and redirects
//! itself and would otherwise leak `X-API-Key`/`X-API-Hash` across an
//! `Authorization`-stripping-only reqwest redirect.

use crate::error::OpenQAError;
#[cfg(test)]
use crate::http::{HttpClient, VerifyPolicy};
use crate::http::{MAX_API_BODY, sanitize_url};

/// Builds a [`ruoqa::Client`] for `base_url` with a fresh, single-use
/// transport.
///
/// Test/fixture scaffolding only: every production call site
/// (`Session::openqa_transport` and its `mtui-core` callers) builds the
/// transport once and reuses it across hosts via
/// [`build_openqa_client_with_transport`] directly, so the connection pool set
/// up by D1's dedicated openQA transport is actually shared. Kept
/// crate-internal so it can't be reached for that (wrong, pool-defeating)
/// purpose from outside this crate.
///
/// # Errors
///
/// See [`build_openqa_client_with_transport`], plus [`OpenQAError::Http`] if
/// the transport itself fails to build (e.g. an unreadable CA bundle).
#[cfg(test)]
pub(crate) fn build_openqa_client(
    verify: VerifyPolicy,
    base_url: &str,
) -> Result<ruoqa::Client, OpenQAError> {
    build_openqa_client_with_transport(HttpClient::openqa_transport(verify)?, base_url)
}

/// Builds a [`ruoqa::Client`] for `base_url`, injecting an already-built
/// `transport` (see [`crate::http::HttpClient::openqa_transport`]).
///
/// Credentials are resolved by `ruoqa` itself from the standard
/// `client.conf`/`$OPENQA_CONFIG` search path (see the crate docs), keyed on
/// `base_url`'s host. mtui's own ruoqa-level retries are disabled
/// (`max_retries(0)`): a flaky openQA is mtui's caller's problem to retry or
/// fold to "no results" (see [`super::base::OpenQABase::get_jobs`]), not
/// something to hide behind an opaque, cancellation-unaware retry loop.
///
/// # Errors
///
/// Returns [`OpenQAError::ClientBuild`] if `ruoqa` fails to build the client
/// (a malformed `client.conf`, or an invalid `User-Agent`/API key).
pub fn build_openqa_client_with_transport(
    transport: reqwest::Client,
    base_url: &str,
) -> Result<ruoqa::Client, OpenQAError> {
    ruoqa::ClientBuilder::new()
        .server(base_url)
        .http_client(transport)
        .retry(ruoqa::RetryPolicy::default().max_retries(0).deadline(None))
        .max_response_bytes(MAX_API_BODY)
        .build()
        .map_err(|e| OpenQAError::ClientBuild(describe(&e)))
}

/// Renders a [`ruoqa::Error`] for [`OpenQAError`]: userinfo cannot reach this
/// message — `ruoqa` >= 0.1.4 redacts it at every `Display` site
/// (`Request`/`Connection`/`CrossOriginRedirect` all render via its
/// `RedactedUrl`) and drops it in `config::resolve`, and this function never
/// renders a `reqwest::Error` directly (see [`root_cause`]).
pub(crate) fn describe(e: &ruoqa::Error) -> String {
    match e {
        // `Connection`'s Display is just "failed to connect to
        // <redacted-url>" — ruoqa redacts the URL but drops the transport
        // cause entirely. Recover it from the source chain: rendering the
        // `reqwest::Error` itself would append an unredacted `for url (...)`
        // (reqwest-0.13.4/src/error.rs:280).
        ruoqa::Error::Connection { source, .. } => match root_cause(source) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        },
        // Every other variant already interpolates its own source
        // (`Config`/`Tls`/`Parse`) or carries none.
        other => other.to_string(),
    }
}

/// Walks `e`'s source chain to the deepest cause, deliberately skipping `e`
/// itself (whose `Display` may be unredacted — see [`describe`]), and returns
/// it through [`sanitize_url`] as a backstop against a future layer embedding
/// a URL.
///
/// Returns `None` if `e` carries no source at all.
fn root_cause(e: &dyn std::error::Error) -> Option<String> {
    let mut deepest = e.source()?;
    while let Some(next) = deepest.source() {
        deepest = next;
    }
    Some(sanitize_url(&deepest.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Also the regression pin for "`build_openqa_client_with_transport` never
    /// calls `.tls()`/`.timeouts()`": either is a compile-time-silent, only-
    /// fails-at-`build()` `Error::IncompatibleHttpClient` once an
    /// `http_client` is injected (ruoqa's own contract, exercised directly in
    /// `tests/openqa.rs::injected_transport_rejects_tls_override`/
    /// `_timeouts_override`), so this assertion turning `Err` is what actually
    /// catches a future edit re-adding either call here. Observed red: adding
    /// `.tls(TlsMode::danger_accept_invalid_certs())` to
    /// `build_openqa_client_with_transport` fails this assertion.
    #[test]
    fn build_openqa_client_succeeds_with_default_verify() {
        // No `client.conf` in this process's search path is assumed; a bad
        // config would surface as `ClientBuild`, exercised via the
        // integration suite instead (needs a real filesystem fixture).
        let client = build_openqa_client(VerifyPolicy::Default(true), "openqa.example.com");
        assert!(client.is_ok());
    }

    // `describe` itself is exercised end-to-end against a *real*
    // `ruoqa::Error::Connection`/`CrossOriginRedirect` in `tests/openqa.rs`:
    // `ruoqa::Error`'s variants are `#[non_exhaustive]`, so this crate cannot
    // construct one directly to unit-test `describe`'s match arms in
    // isolation. `root_cause` has no such restriction, since it walks a
    // plain `&dyn Error`.

    #[derive(Debug)]
    struct Leaf(&'static str);
    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for Leaf {}

    #[derive(Debug)]
    struct Wrapper {
        source: Box<dyn std::error::Error>,
    }
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "wrapper")
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    /// Two-deep chain: `root_cause` must sanitize the deepest message.
    /// Deleting the `sanitize_url` call in `root_cause` turns this red.
    #[test]
    fn root_cause_sanitizes_the_deepest_message() {
        let e = Wrapper {
            source: Box::new(Leaf("boom https://alice:s3cret@h/x")),
        };
        assert_eq!(root_cause(&e).as_deref(), Some("boom https://***@h/x"));
    }

    /// Three-deep chain: `root_cause` must return the *deepest* message, not
    /// an intermediate one. Changing `root_cause` to stop at the first
    /// `source()` instead of walking to the end turns this red.
    #[test]
    fn root_cause_returns_the_deepest_not_the_intermediate() {
        let e = Wrapper {
            source: Box::new(Wrapper {
                source: Box::new(Leaf("deepest")),
            }),
        };
        assert_eq!(root_cause(&e).as_deref(), Some("deepest"));
    }

    /// An error with no source at all yields `None`, so callers fall back to
    /// the outer error's own (already-redacted) `Display`.
    #[test]
    fn root_cause_is_none_without_a_source() {
        assert_eq!(root_cause(&Leaf("no source here")), None);
    }
}
