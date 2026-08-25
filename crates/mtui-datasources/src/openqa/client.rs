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
use crate::http::{MAX_API_BODY, root_cause};

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
    ///
    /// `#[serial(openqa_config_env)]`-guarded: `ruoqa` 0.3 also resolves
    /// `$OPENQA_API_KEY`/`$OPENQA_API_SECRET`, and exactly one set in the
    /// ambient environment (rather than a matched pair, or neither) would
    /// turn this build into an `Err`.
    #[test]
    #[serial_test::serial(openqa_config_env)]
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // `#[serial(openqa_config_env)]` guard makes the mutation exclusive.
    #[allow(unsafe_code)]
    fn build_openqa_client_succeeds_with_default_verify() {
        // No `client.conf` in this process's search path is assumed; a bad
        // config would surface as `ClientBuild`, exercised via the
        // integration suite instead (needs a real filesystem fixture).
        let prev_key = std::env::var_os("OPENQA_API_KEY");
        let prev_secret = std::env::var_os("OPENQA_API_SECRET");
        // SAFETY: guarded by `#[serial(openqa_config_env)]`.
        unsafe {
            std::env::remove_var("OPENQA_API_KEY");
            std::env::remove_var("OPENQA_API_SECRET");
        }
        let client = build_openqa_client(VerifyPolicy::Default(true), "openqa.example.com");
        // SAFETY: guarded by `#[serial(openqa_config_env)]`.
        unsafe {
            match prev_key {
                Some(v) => std::env::set_var("OPENQA_API_KEY", v),
                None => std::env::remove_var("OPENQA_API_KEY"),
            }
            match prev_secret {
                Some(v) => std::env::set_var("OPENQA_API_SECRET", v),
                None => std::env::remove_var("OPENQA_API_SECRET"),
            }
        }
        assert!(client.is_ok());
    }

    // `describe` itself is exercised end-to-end against a *real*
    // `ruoqa::Error::Connection`/`CrossOriginRedirect` in `tests/openqa.rs`:
    // `ruoqa::Error`'s variants are `#[non_exhaustive]`, so this crate cannot
    // construct one directly to unit-test `describe`'s match arms in
    // isolation. `root_cause` has no such restriction, since it walks a plain
    // `&dyn Error` — its own unit tests live with it in `crate::http`.
}
