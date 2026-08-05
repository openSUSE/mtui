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
        .map_err(|e| OpenQAError::ClientBuild(redact(&e)))
}

/// Redacts any URL userinfo from a [`ruoqa::Error`]'s rendered message before
/// it reaches [`OpenQAError`].
///
/// Three of `ruoqa::Error`'s variants embed a `url::Url` in their `Display`:
/// `Request`, `Connection`, and — the one path that can actually carry a
/// *server-supplied* credential rather than mtui's own — `CrossOriginRedirect`,
/// whose `to` comes straight from a `Location` header a malicious or
/// misconfigured openQA instance controls. Each is rebuilt here with its
/// URL(s) run through [`sanitize_url`] individually, rather than sanitizing
/// the whole rendered string: `Display` for `CrossOriginRedirect` embeds
/// *two* URLs, and [`sanitize_url`] only recognises a single bare
/// `scheme://[user:pass@]host` shape — scanning the concatenated message finds
/// the first URL (never credentialed here: it's `ruoqa`'s own request URL),
/// authorises on it, and returns the *original, unmodified* string, silently
/// skipping the second, credentialed one. Every other variant renders as-is:
/// none of them embed a URL, per `ruoqa::error`'s source.
pub(crate) fn redact(e: &ruoqa::Error) -> String {
    match e {
        ruoqa::Error::Request {
            method,
            url,
            status,
            ..
        } => {
            format!("{method} {} returned {status}", sanitize_url(url.as_str()))
        }
        ruoqa::Error::Connection { url, source } => {
            format!(
                "failed to connect to {}: {}",
                sanitize_url(url.as_str()),
                sanitize_url(&source.to_string())
            )
        }
        ruoqa::Error::CrossOriginRedirect { from, to } => {
            format!(
                "refusing to follow redirect from {} to a different origin {}",
                sanitize_url(from.as_str()),
                sanitize_url(to.as_str())
            )
        }
        other => other.to_string(),
    }
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

    // `redact` itself is exercised end-to-end against a *real*
    // `ruoqa::Error::CrossOriginRedirect` (the one variant that can carry a
    // server-supplied credential) in
    // `tests/openqa.rs::try_get_jobs_error_never_leaks_a_credential_from_a_redirect_location`:
    // `ruoqa::Error`'s variants are `#[non_exhaustive]`, so this crate cannot
    // construct one directly to unit-test `redact`'s match arms in isolation.
}
