//! Builds the [`ruoqa::Client`] used by the openQA connectors.
//!
//! `ruoqa` owns the whole signed-request contract: INI `client.conf` /
//! `$OPENQA_CONFIG` discovery, HMAC-SHA1 `X-API-Hash`, retries and
//! same-origin-only redirects. All that is left here is wiring mtui's shared
//! TLS/timeout policy into it as an injected, redirect-less, no-reqwest-retry
//! `reqwest::Client` (see [`crate::http::HttpClient::openqa_transport`]),
//! because `ruoqa` re-signs and redirects itself and would otherwise leak
//! `X-API-Key`/`X-API-Hash` across an `Authorization`-stripping-only reqwest
//! redirect.

use crate::error::OpenQAError;
#[cfg(test)]
use crate::http::{HttpClient, VerifyPolicy};
use crate::http::{MAX_API_BODY, root_cause};

/// Builds a [`ruoqa::Client`] for `base_url` with a fresh, single-use
/// transport.
///
/// Test/fixture scaffolding only, and crate-internal so it cannot be reached
/// for anything else: every production call site (`Session::openqa_transport`
/// and its `mtui-core` callers) builds the transport once and reuses it across
/// hosts via [`build_openqa_client_with_transport`], so the connection pool is
/// actually shared.
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
/// `ruoqa` resolves credentials itself from the standard
/// `client.conf`/`$OPENQA_CONFIG` search path, keyed on `base_url`'s host.
/// Its retries are disabled (`max_retries(0)`): a flaky openQA is the caller's
/// problem to retry or fold to "no results" (see
/// [`super::base::OpenQABase::get_jobs`]), not something to hide behind an
/// opaque, cancellation-unaware retry loop.
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
        // `Connection`'s Display is only "failed to connect to
        // <redacted-url>": ruoqa redacts the URL but drops the transport cause.
        // Recover it from the source chain — rendering the `reqwest::Error`
        // itself would append an unredacted ` for url (...)`.
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
    /// calls `.tls()`/`.timeouts()`": with an `http_client` injected, either is
    /// a compile-silent `Error::IncompatibleHttpClient` at `build()` (ruoqa's
    /// contract, exercised directly in `tests/openqa.rs`), so this assertion
    /// going `Err` is what catches a future edit re-adding one. Observed red by
    /// adding `.tls(TlsMode::danger_accept_invalid_certs())`.
    ///
    /// `#[serial(openqa_config_env)]`-guarded: `ruoqa` 0.3 also resolves
    /// `$OPENQA_API_KEY`/`$OPENQA_API_SECRET`, and exactly one of the pair set
    /// in the ambient environment would turn this build into an `Err`.
    #[test]
    #[serial_test::serial(openqa_config_env)]
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // `#[serial(openqa_config_env)]` guard makes the mutation exclusive.
    #[allow(unsafe_code)]
    fn build_openqa_client_succeeds_with_default_verify() {
        // Assumes no `client.conf` in this process's search path; a bad config
        // surfaces as `ClientBuild`, exercised in the integration suite.
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

    // `ruoqa::Error`'s variants are `#[non_exhaustive]`, so this crate cannot
    // construct one to unit-test `describe`'s match arms; it is exercised
    // end-to-end against real errors in `tests/openqa.rs` instead.
}
