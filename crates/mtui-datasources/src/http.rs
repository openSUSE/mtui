//! Single source of truth for outbound HTTP timeout and TLS policy.
//!
//! Every place in mtui that talks to an HTTP(S) service (the Gitea PR client,
//! the QEM Dashboard client, the openQA / QAM Dashboard search, the openQA
//! install/result-log downloads, and the `refhosts.yml` fetch) shares one
//! `(connect, read)` timeout and one TLS-verification policy instead of each
//! making its own, inconsistent decision. This module centralises both:
//!
//! - `HTTP_TIMEOUT` is the one `(connect, read)` timeout shared by all
//!   callers. It bounds a stuck socket so a broken network cannot hang mtui.
//! - [`resolve_verify`] turns a per-call default plus the user's global
//!   preference into the effective [`VerifyPolicy`].
//! - [`HttpClient`] builds one `reqwest::Client` with a fixed timeout + verify
//!   posture. Verification is **on by default for every call site.**
//! - `is_ssl_verification_error` / `ssl_verification_hint` /
//!   `ssl_error_detail` give a short, actionable message for the internal-CA
//!   hosts (openqa.suse.de, dashboard.qam.suse.de, ...) instead of a raw
//!   transport error.
//! - URL redaction has three legs, in order of load-bearing-ness (#431): the
//!   primary one is the strip at the [`HttpError`] conversion boundary, which
//!   most call sites (teregen, the QEM dashboard) rely on and nothing else;
//!   `sanitize_url` is for interpolating a request URL into a message as
//!   context; and `root_cause` reaches past an error layer whose own `Display`
//!   may embed a URL, for a detail line.
//!
//! ## Client construction and verify precedence
//!
//! reqwest fixes the TLS posture when the `Client` is built and reuses one
//! connection pool across calls, so a caller resolves the effective
//! [`VerifyPolicy`] with [`resolve_verify`] before constructing the client via
//! [`HttpClient::new`]`(verify)`, then issues requests through
//! [`HttpClient::get_bytes`]`(url)`.
//!
//! The config-string coercion for `ssl_verify` lives in
//! [`mtui_config::SslVerify`]; [`VerifyPolicy::from_config`] bridges that typed
//! config value into this layer's posture.
//!
//! ## Bounded response bodies
//!
//! Every body is read through `read_body_capped`, which rejects an oversized advertised
//! `Content-Length` before reading and enforces the same ceiling while streaming
//! chunked/unknown-length bodies — a hostile or misconfigured datasource cannot
//! OOM mtui. Callers pick [`MAX_API_BODY`] (small JSON/text) or
//! `MAX_DOWNLOAD_BODY` (bulk downloads); overflow surfaces as
//! [`HttpError::BodyTooLarge`], which carries no URL so it cannot leak a
//! credential embedded in a datasource URL.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mtui_config::SslVerify;

use crate::error::{HttpError, Result};

/// Shared `(connect, read)` timeout for every outbound HTTP call: 5s to
/// connect, 30s to read. Bounds a stuck socket so a broken network can't hang
/// mtui indefinitely.
pub(crate) const HTTP_TIMEOUT: (Duration, Duration) =
    (Duration::from_secs(5), Duration::from_secs(30));

/// The connect-phase component of [`HTTP_TIMEOUT`].
const CONNECT_TIMEOUT: Duration = HTTP_TIMEOUT.0;
/// The read-phase component of [`HTTP_TIMEOUT`].
const READ_TIMEOUT: Duration = HTTP_TIMEOUT.1;

/// Maximum body size for a JSON/text API response (Gitea, openQA, QEM
/// dashboard, OBS/IBS, teregen, oqa-search). These endpoints return small
/// structured documents; 16 MiB is orders of magnitude above any legitimate
/// payload while still bounding a hostile/misconfigured server. See
/// [`HttpClient::get_bytes_capped`] and the crate's direct-`Response` callers.
pub const MAX_API_BODY: usize = 16 * 1024 * 1024;

/// Maximum body size for a bulk *download* — a `refhosts.yml` database or an
/// openQA install/result log fetched via [`HttpClient::get_bytes`]. Generous
/// (256 MiB) because these can legitimately be large, but still a hard ceiling
/// so a runaway body cannot exhaust memory.
const MAX_DOWNLOAD_BODY: usize = 256 * 1024 * 1024;

/// Fallback locations of the distribution-managed CA bundle, probed only when
/// the interpreter/OpenSSL default (via `SSL_CERT_FILE`) names no existing file.
const SYSTEM_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/ca-bundle.pem",             // openSUSE / SLE
    "/etc/ssl/certs/ca-certificates.crt", // Debian / Ubuntu
    "/etc/pki/tls/certs/ca-bundle.crt",   // Fedora / RHEL
    "/etc/ssl/cert.pem",                  // Alpine, BSDs
];

/// Process-wide idempotency flag for the "verification disabled" warning: the
/// first call warns and every later call is a no-op.
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);

/// The default connection-pool width (`min(32, cpu + 4)`) so concurrent
/// fan-out at a single host reuses connections instead of churning the pool.
#[must_use]
fn default_pool_size() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    (cpus + 4).min(32)
}

/// A `reqwest`-compatible TLS verification posture: verify against the system
/// trust store (`Default(true)`), skip verification (`Default(false)`), or
/// verify against a specific CA bundle file (`CaBundle`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// Verify (`true`) or skip (`false`) using the default trust store.
    Default(bool),
    /// Verify against the CA bundle / certificate at this path.
    CaBundle(PathBuf),
}

impl VerifyPolicy {
    /// Bridge a typed [`mtui_config::SslVerify`] into this layer's posture.
    ///
    /// - [`SslVerify::Enabled`] → verify, preferring the system CA bundle
    ///   (`system_ca_bundle`) when one is found (the distribution bundle
    ///   takes precedence over any bundled default), else `Default(true)`.
    /// - [`SslVerify::Disabled`] → `Default(false)`.
    /// - [`SslVerify::CaBundle`] → the configured path verbatim.
    #[must_use]
    pub fn from_config(verify: &SslVerify) -> Self {
        Self::from_config_with_bundle(verify, system_ca_bundle())
    }

    /// Pure core of [`from_config`](Self::from_config): map an [`SslVerify`]
    /// given an already-resolved system CA bundle. Extracted so the mapping is
    /// testable without touching the process-global `SSL_CERT_FILE`.
    #[must_use]
    fn from_config_with_bundle(verify: &SslVerify, system_bundle: Option<PathBuf>) -> Self {
        match verify {
            SslVerify::Enabled => match system_bundle {
                Some(bundle) => Self::CaBundle(bundle),
                None => Self::Default(true),
            },
            SslVerify::Disabled => Self::Default(false),
            SslVerify::CaBundle(path) => Self::CaBundle(path.clone()),
        }
    }
}

/// Pick the effective verify value for a request.
///
/// `override_` wins whenever it is `Some`; otherwise the per-call `default`
/// (call sites pass `Default(true)` so verification is on unless the user has
/// expressed a preference) is used.
#[must_use]
pub fn resolve_verify(default: VerifyPolicy, override_: Option<VerifyPolicy>) -> VerifyPolicy {
    override_.unwrap_or(default)
}

/// Silence the "verification disabled" warning once, idempotently.
///
/// The first call warns process-wide and later calls are cheap no-ops. Called
/// only when verification is deliberately disabled, so REPL output stays
/// readable.
fn disable_insecure_warnings() {
    if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            "TLS certificate verification is disabled (ssl_verify = false); \
             connections are not authenticated"
        );
    }
}

/// The timeout + TLS-verify posture shared by every `reqwest::Client` this
/// crate builds: [`HttpClient::new`] and [`HttpClient::openqa_transport`].
/// Kept as a single function so the two clients' TLS/timeout behaviour cannot
/// drift apart.
fn base_builder(verify: VerifyPolicy) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .pool_max_idle_per_host(default_pool_size());

    builder = match verify {
        VerifyPolicy::Default(true) => builder,
        VerifyPolicy::Default(false) => {
            disable_insecure_warnings();
            builder.danger_accept_invalid_certs(true)
        }
        VerifyPolicy::CaBundle(path) => {
            let certs = load_ca_bundle(&path)?;
            builder.tls_certs_only(certs)
        }
    };
    Ok(builder)
}

/// A shared outbound HTTP client with a fixed timeout and TLS-verify posture.
///
/// Built once and reused so concurrent fan-out at a single host reuses pooled
/// connections.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Build a client with the given [`VerifyPolicy`].
    ///
    /// Applies `HTTP_TIMEOUT` (connect + read), sizes the per-host idle pool
    /// to `default_pool_size`, and derives the TLS posture:
    ///
    /// - `Default(true)` → verify against the built-in trust store;
    /// - `Default(false)` → accept invalid certs (and warn once via
    ///   `disable_insecure_warnings`);
    /// - `CaBundle(path)` → verify against *only* that PEM bundle (replacing,
    ///   not augmenting, the trust store).
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub fn new(verify: VerifyPolicy) -> Result<Self> {
        Ok(Self {
            inner: base_builder(verify)?.build()?,
        })
    }

    /// Builds a dedicated `reqwest::Client` for injection into
    /// [`ruoqa::ClientBuilder::http_client`](https://docs.rs/ruoqa).
    ///
    /// Shares this crate's timeout/TLS posture (via the crate-internal
    /// `base_builder`) but is
    /// **not** part of the shared connection pool: it additionally disables
    /// redirects and reqwest-level retries, which `ruoqa` requires of an
    /// injected client — it re-signs every request and follows same-origin
    /// redirects itself, so a reqwest-level redirect would forward the
    /// `X-API-Key`/`X-API-Hash` headers cross-origin (reqwest only strips
    /// `Authorization`/`Cookie`/`Proxy-Authorization`), and a reqwest-level
    /// retry would replay a stale signature.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub fn openqa_transport(verify: VerifyPolicy) -> Result<reqwest::Client> {
        Ok(base_builder(verify)?
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?)
    }

    /// Borrow the underlying `reqwest::Client` for callers that need the full
    /// request surface (JSON, headers, POST) rather than a bare GET.
    #[must_use]
    pub(crate) fn inner(&self) -> &reqwest::Client {
        &self.inner
    }

    /// GET `url` and return the raw response body as bytes, capped at
    /// `MAX_DOWNLOAD_BODY`.
    ///
    /// The single GET-to-bytes path for callers that just want a payload (a log
    /// file, a YAML document). Errors on any non-2xx status. Use
    /// [`get_bytes_capped`](Self::get_bytes_capped) with [`MAX_API_BODY`] for
    /// small JSON/text API responses.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Request`] on any transport failure or non-2xx HTTP
    /// status, or [`HttpError::BodyTooLarge`] if the body exceeds the cap.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.get_bytes_capped(url, MAX_DOWNLOAD_BODY).await
    }

    /// GET `url` and return the response body as bytes, rejecting any body
    /// larger than `max`.
    ///
    /// An oversized advertised `Content-Length` is rejected *before* any body
    /// is read; a chunked/unknown-length body is streamed and rejected the
    /// instant the running total exceeds `max`, so an endless or
    /// `Content-Length`-lying server cannot force unbounded buffering.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Request`] on any transport failure or non-2xx HTTP
    /// status, or [`HttpError::BodyTooLarge`] if the body exceeds `max`.
    pub async fn get_bytes_capped(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        let response = self.inner.get(url).send().await?.error_for_status()?;
        read_body_capped(response, max).await
    }
}

/// Read `response`'s body into a `Vec`, rejecting any body larger than `max`.
///
/// Fail-fast on the advertised `Content-Length`, then enforce the same ceiling
/// while streaming so a missing/lying length or an endless chunked body still
/// cannot exceed `max` bytes buffered. Dropping `response` on rejection closes
/// the underlying connection instead of draining the overflow.
pub(crate) async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length()
        && len > max as u64
    {
        return Err(HttpError::BodyTooLarge {
            limit: max,
            seen: Some(len),
        });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(HttpError::BodyTooLarge {
                limit: max,
                seen: None,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Read a PEM CA bundle from `path` into reqwest certificates.
fn load_ca_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>> {
    let pem = std::fs::read(path).map_err(|source| HttpError::CaBundle {
        path: path.display().to_string(),
        source,
    })?;
    // `HttpError::from`, never the bare variant: the URL strip lives in the
    // conversion (see the variant's invariant).
    reqwest::Certificate::from_pem_bundle(&pem).map_err(HttpError::from)
}

/// The bound both source-chain walks below use to guard against a
/// self-referential chain looping forever. Shared so the two cannot drift
/// apart — see [`root_cause`]'s doc for how the two loops differ in effect
/// while sharing this bound.
const MAX_ERROR_SOURCE_LINKS: usize = 64;

/// Return `true` if `err` is (or was caused by) a TLS certificate-verification
/// failure.
///
/// reqwest wraps the underlying rustls verification error several layers deep,
/// so this walks the
/// [`std::error::Error::source`] chain (guarding against cycles) and falls back
/// to matching `CERTIFICATE_VERIFY_FAILED` / a certificate-verify phrase in the
/// stringified error for transports that only surface it in the message.
#[must_use]
pub(crate) fn is_ssl_verification_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut seen = 0usize;
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        // Guard against a self-referential source chain looping forever.
        if seen > MAX_ERROR_SOURCE_LINKS {
            break;
        }
        seen += 1;
        let text = e.to_string();
        if text.contains("CERTIFICATE_VERIFY_FAILED")
            || text.contains("certificate verify failed")
            || text.contains("invalid peer certificate")
        {
            return true;
        }
        current = e.source();
    }
    let text = err.to_string();
    text.contains("CERTIFICATE_VERIFY_FAILED") || text.contains("certificate verify failed")
}

/// A short, actionable message for a TLS certificate-verification failure.
///
/// Names the concrete remedies instead of dumping a transport error. The
/// custom-bundle example stays
/// generic on purpose — naming the system bundle would suggest a no-op, since it
/// is usually already the verify source that just failed.
#[must_use]
pub(crate) fn ssl_verification_hint(host: Option<&str>) -> String {
    let where_ = host.map(|h| format!(" to {h}")).unwrap_or_default();
    format!(
        "TLS certificate verification failed{where_}. The server's certificate \
         could not be verified against the trusted CA bundle. To fix this, \
         install the missing CA (e.g. the SUSE root CA) into your system trust \
         store, point 'ssl_verify' at a CA bundle file that contains the \
         server's CA ('ssl_verify = /path/to/ca.pem' under the [mtui] section \
         of your mtui config, e.g. ~/.config/mtui/mtui.toml), or disable \
         verification there with 'ssl_verify = false'."
    )
}

/// Return `url` with any userinfo (`user[:password]@`) replaced by `***`, for
/// safe logging/diagnostics.
///
/// Configured datasource URLs may legitimately embed credentials
/// (`scheme://user:pass@host/…`); those must never reach the log stream. This
/// preserves the scheme, host, port, path and query so a log line stays useful,
/// and touches only the authority's userinfo. Parsing is deliberately
/// dependency-free and mirrors `host_of`-style string splitting; on any
/// unexpected shape it fails closed by stripping an entire `…@` authority prefix
/// rather than risk echoing a credential.
#[must_use]
pub(crate) fn sanitize_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // No scheme separator: if there is an `@`, drop everything up to and
        // including it (fail closed); otherwise the input carries no userinfo.
        return match rest_after_at(url) {
            Some(after) => format!("***@{after}"),
            None => url.to_owned(),
        };
    };
    // The authority ends at the first path/query/fragment delimiter.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_userinfo, hostport)) => format!("{scheme}://***@{hostport}{tail}"),
        None => url.to_owned(),
    }
}

/// If `s` contains an `@`, return the slice after the last one; else `None`.
fn rest_after_at(s: &str) -> Option<&str> {
    s.rsplit_once('@').map(|(_, after)| after)
}

/// The DEBUG-level detail line accompanying [`ssl_verification_hint`]: the
/// deepest cause of `e`, or `e`'s own `Display` when it has no source.
///
/// Taking [`HttpError`] rather than `&dyn Error` is the point: it makes "the
/// error has already crossed the conversion boundary" a compile-time
/// precondition, so neither branch can render a URL (#431). `HttpError::Request`
/// is `#[error(transparent)]`, so `source()` forwards past reqwest's own
/// `Display` straight to the rustls/IO cause — which is also the actionable
/// part, reqwest's `Display` saying only "error sending request".
#[must_use]
pub(crate) fn ssl_error_detail(e: &HttpError) -> String {
    root_cause(e).unwrap_or_else(|| e.to_string())
}

/// Walks `e`'s source chain to the deepest cause, deliberately skipping `e`
/// itself (whose `Display` may embed a URL — reqwest's appends the request URL,
/// `ruoqa::Error::Connection`'s renders a redacted one), and returns it through
/// [`sanitize_url`] as a backstop.
///
/// That backstop handles a **single** embedded URL only: `sanitize_url` parses
/// one `scheme://[userinfo@]host` and silently no-ops on a second one in the
/// same string. It is a safety net for an unexpected layer, not a substitute
/// for stripping at the boundary.
///
/// Returns `None` if `e` carries no source at all, so callers fall back to the
/// outer error's own `Display`. A self-referential source chain is bounded like
/// [`is_ssl_verification_error`]'s walk, sharing [`MAX_ERROR_SOURCE_LINKS`],
/// though not identically: that one inspects at most 65 links, this one
/// advances at most 66 times past the first source. Either bound only has to
/// terminate. (The openQA copy this was hoisted from had no bound at all.)
#[must_use]
pub(crate) fn root_cause(e: &dyn std::error::Error) -> Option<String> {
    let mut deepest = e.source()?;
    let mut seen = 0usize;
    while let Some(next) = deepest.source() {
        if seen > MAX_ERROR_SOURCE_LINKS {
            break;
        }
        seen += 1;
        deepest = next;
    }
    Some(sanitize_url(&deepest.to_string()))
}

/// The system's CA bundle path, or `None` when none is found.
///
/// Prefers the `SSL_CERT_FILE` environment override (an OpenSSL-standard
/// default-cafile variable), then the well-known distribution paths in
/// [`SYSTEM_CA_BUNDLES`]. Returns the first candidate that is an existing
/// file.
#[must_use]
fn system_ca_bundle() -> Option<PathBuf> {
    let env_cafile = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    resolve_ca_bundle(env_cafile, SYSTEM_CA_BUNDLES)
}

/// Pure core of [`system_ca_bundle`]: return the first existing file from the
/// env override (if any) followed by the fallback list. Extracted so the
/// selection logic is testable without mutating the process-global
/// `SSL_CERT_FILE` (unsound under parallel tests in edition 2024).
fn resolve_ca_bundle(env_cafile: Option<PathBuf>, fallbacks: &[&str]) -> Option<PathBuf> {
    let candidates = env_cafile
        .into_iter()
        .chain(fallbacks.iter().map(PathBuf::from));
    candidates.into_iter().find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset the process-wide insecure-warning flag around a test so the
    /// idempotency assertions are deterministic regardless of order.
    fn reset_insecure_warned() {
        INSECURE_WARNED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn http_timeout_is_positive_connect_read_tuple() {
        let (connect, read) = HTTP_TIMEOUT;
        assert!(connect > Duration::ZERO);
        assert!(read > Duration::ZERO);
    }

    #[test]
    fn default_pool_size_is_bounded() {
        // Cannot mock available_parallelism, but the invariant
        // (min(32, cpu + 4)) means the result is always in [5, 32].
        let n = default_pool_size();
        assert!((5..=32).contains(&n), "pool size {n} out of [5, 32]");
    }

    #[test]
    fn resolve_verify_precedence() {
        // (default, override, expected) test cases.
        let f = || VerifyPolicy::Default(false);
        let t = || VerifyPolicy::Default(true);
        let ca = || VerifyPolicy::CaBundle(PathBuf::from("/etc/ssl/ca.pem"));

        assert_eq!(resolve_verify(f(), None), f()); // unset -> keep default
        assert_eq!(resolve_verify(t(), None), t());
        assert_eq!(resolve_verify(f(), Some(t())), t()); // override wins
        assert_eq!(resolve_verify(t(), Some(f())), f());
        assert_eq!(resolve_verify(f(), Some(ca())), ca()); // CA bundle path
    }

    #[test]
    fn from_config_disabled_maps_to_default_false() {
        assert_eq!(
            VerifyPolicy::from_config(&SslVerify::Disabled),
            VerifyPolicy::Default(false)
        );
    }

    #[test]
    fn from_config_ca_bundle_is_verbatim() {
        let p = PathBuf::from("/my/own/cert.pem");
        assert_eq!(
            VerifyPolicy::from_config(&SslVerify::CaBundle(p.clone())),
            VerifyPolicy::CaBundle(p)
        );
    }

    #[test]
    fn from_config_enabled_uses_system_bundle_when_present() {
        // Enabled + a resolved system bundle -> verify against that bundle
        // (the system bundle takes precedence).
        let ca = PathBuf::from("/sys/ca.pem");
        assert_eq!(
            VerifyPolicy::from_config_with_bundle(&SslVerify::Enabled, Some(ca.clone())),
            VerifyPolicy::CaBundle(ca)
        );
    }

    #[test]
    fn from_config_enabled_falls_back_to_default_true_without_bundle() {
        // Enabled + no system bundle -> Default(true) (still verifying).
        assert_eq!(
            VerifyPolicy::from_config_with_bundle(&SslVerify::Enabled, None),
            VerifyPolicy::Default(true)
        );
    }

    #[test]
    fn resolve_ca_bundle_prefers_env_cafile() {
        // The env candidate wins over the fallback list when it exists.
        let dir = tempfile::tempdir().unwrap();
        let cafile = dir.path().join("openssl.pem");
        std::fs::write(&cafile, "dummy").unwrap();
        let fallback = dir.path().join("fallback.pem");
        std::fs::write(&fallback, "dummy").unwrap();
        let fallback_s = fallback.to_str().unwrap();

        assert_eq!(
            resolve_ca_bundle(Some(cafile.clone()), &[fallback_s]),
            Some(cafile)
        );
    }

    #[test]
    fn resolve_ca_bundle_falls_back_to_wellknown_paths() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.pem");
        let present = dir.path().join("present.pem");
        std::fs::write(&present, "dummy").unwrap();
        let present_s = present.to_str().unwrap();

        // env candidate missing -> fall through to the first existing fallback.
        assert_eq!(
            resolve_ca_bundle(
                Some(missing),
                &[dir.path().join("no.pem").to_str().unwrap(), present_s]
            ),
            Some(present)
        );
    }

    #[test]
    fn resolve_ca_bundle_none_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.pem");
        let fb = dir.path().join("also-nope.pem");
        assert_eq!(
            resolve_ca_bundle(Some(missing), &[fb.to_str().unwrap()]),
            None
        );
    }

    #[test]
    fn resolve_ca_bundle_none_when_no_env_and_empty_fallbacks() {
        assert_eq!(resolve_ca_bundle(None, &[]), None);
    }

    #[test]
    fn disable_insecure_warnings_is_idempotent() {
        reset_insecure_warned();
        // First call flips the flag; subsequent calls are no-ops. We assert the
        // flag transition rather than capturing the log line.
        assert!(!INSECURE_WARNED.load(Ordering::SeqCst));
        disable_insecure_warnings();
        assert!(INSECURE_WARNED.load(Ordering::SeqCst));
        disable_insecure_warnings();
        assert!(INSECURE_WARNED.load(Ordering::SeqCst));
    }

    #[test]
    fn is_ssl_verification_error_matches_message_fallback() {
        #[derive(Debug)]
        struct CertErr;
        impl std::fmt::Display for CertErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "... CERTIFICATE_VERIFY_FAILED ...")
            }
        }
        impl std::error::Error for CertErr {}
        assert!(is_ssl_verification_error(&CertErr));
    }

    #[test]
    fn is_ssl_verification_error_detects_wrapped_source() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "certificate verify failed: self-signed")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "wrapped transport error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        assert!(is_ssl_verification_error(&Outer(Inner)));
    }

    #[test]
    fn is_ssl_verification_error_false_for_other_errors() {
        #[derive(Debug)]
        struct Timeout;
        impl std::fmt::Display for Timeout {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "operation timed out")
            }
        }
        impl std::error::Error for Timeout {}
        assert!(!is_ssl_verification_error(&Timeout));
    }

    #[test]
    fn ssl_verification_hint_mentions_remedies() {
        let msg = ssl_verification_hint(Some("src.suse.de"));
        assert!(msg.contains("src.suse.de"));
        assert!(msg.contains("ssl_verify = false"));
        assert!(msg.contains("CA"));
    }

    #[test]
    fn ssl_verification_hint_without_host_and_generic_bundle() {
        let msg = ssl_verification_hint(None);
        assert!(msg.contains("ssl_verify = false"));
        assert!(msg.contains("/path/to/ca.pem"));
        assert!(!msg.contains("ca-bundle.pem"));
    }

    #[test]
    fn sanitize_url_redacts_password() {
        let out = sanitize_url("https://alice:s3cret@host.example/api/v1?q=1");
        assert_eq!(out, "https://***@host.example/api/v1?q=1");
        assert!(!out.contains("s3cret"));
        assert!(!out.contains("alice"));
    }

    #[test]
    fn sanitize_url_redacts_username_only() {
        let out = sanitize_url("https://token@host.example:443/path");
        assert_eq!(out, "https://***@host.example:443/path");
        assert!(!out.contains("token"));
    }

    #[test]
    fn sanitize_url_without_userinfo_is_unchanged() {
        let url = "https://host.example:8080/a/b?c=d#e";
        assert_eq!(sanitize_url(url), url);
    }

    #[test]
    fn sanitize_url_at_in_path_is_not_userinfo() {
        // An `@` after the authority (e.g. in the path) must not be redacted.
        let url = "https://host.example/users/me@example.com";
        assert_eq!(sanitize_url(url), url);
    }

    #[test]
    fn sanitize_url_unparseable_with_at_fails_closed() {
        // No scheme separator but a credential-looking `@`: strip it.
        let out = sanitize_url("alice:s3cret@host.example/x");
        assert_eq!(out, "***@host.example/x");
        assert!(!out.contains("s3cret"));
    }

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

    /// Driven by a *real* `HttpError` rather than a synthetic chain, because
    /// the property under test is a property of the real one: `Request` is
    /// `#[error(transparent)]`, so `source()` forwards past reqwest's own
    /// `Display` — the layer that carries the URL — to the transport cause
    /// underneath. Nothing listens on the loopback discard port (RFC 863).
    #[tokio::test]
    async fn ssl_error_detail_reports_the_transport_cause_without_a_url() {
        let e: HttpError = reqwest::Client::new()
            .get("http://127.0.0.1:9/x")
            .send()
            .await
            .expect_err("connection refused")
            .into();

        let detail = ssl_error_detail(&e);
        assert!(
            !detail.contains(" for url ("),
            "detail rendered reqwest's URL: {detail}"
        );
        // The recovered cause is the actionable half; `e`'s own Display is only
        // "error sending request", which is why the walk exists at all.
        assert!(
            detail.to_lowercase().contains("connection refused"),
            "detail lost the transport cause: {detail}"
        );
        assert_ne!(
            detail,
            e.to_string(),
            "detail must reach past the outer error, not echo it"
        );
    }

    /// A self-referential source chain must terminate rather than hang; the
    /// hop guard bounds the walk as `is_ssl_verification_error`'s does.
    #[test]
    fn root_cause_terminates_on_a_cyclic_chain() {
        #[derive(Debug)]
        struct SelfRef;
        impl std::fmt::Display for SelfRef {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "cycle")
            }
        }
        impl std::error::Error for SelfRef {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&SelfRef)
            }
        }
        assert_eq!(root_cause(&SelfRef).as_deref(), Some("cycle"));
    }

    /// [`HttpClient::openqa_transport`] must not follow a redirect: a
    /// reqwest-level redirect would forward `X-API-Key`/`X-API-Hash` to
    /// whatever `Location` says, off-origin, since reqwest only strips the
    /// standard `Authorization`/`Cookie`/`Proxy-Authorization` headers on a
    /// cross-origin hop. `ruoqa` follows redirects itself (same-origin only),
    /// so the injected client must be a dead end at the first hop.
    #[tokio::test]
    async fn openqa_transport_does_not_follow_redirects() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/end"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = HttpClient::openqa_transport(VerifyPolicy::Default(true)).unwrap();
        let resp = client
            .get(format!("{}/start", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            302,
            "the transport must not follow the redirect itself"
        );
    }
}
