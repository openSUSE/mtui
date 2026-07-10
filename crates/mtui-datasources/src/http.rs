//! Single source of truth for outbound HTTP timeout and TLS policy.
//!
//! Ported from upstream `mtui/support/http.py`. Every place in mtui that talks
//! to an HTTP(S) service (the Gitea PR client, the QEM Dashboard client, the
//! openQA / QAM Dashboard search, the openQA install/result-log downloads, and
//! the `refhosts.yml` fetch) historically defined its own `(connect, read)`
//! timeout and made its own, inconsistent decision about TLS certificate
//! verification. This module centralises both:
//!
//! - [`HTTP_TIMEOUT`] is the one `(connect, read)` timeout shared by all
//!   callers. It bounds a stuck socket so a broken network cannot hang mtui.
//! - [`resolve_verify`] turns a per-call default plus the user's global
//!   preference into the effective [`VerifyPolicy`].
//! - [`HttpClient`] builds one `reqwest::Client` with a fixed timeout + verify
//!   posture. Verification is **on by default for every call site.**
//! - [`is_ssl_verification_error`] / [`ssl_verification_hint`] give a short,
//!   actionable message for the internal-CA hosts (openqa.suse.de,
//!   dashboard.qam.suse.de, ...) instead of a raw transport error.
//!
//! ## Deviation from upstream
//!
//! Python `requests` accepts a `verify=` value per *request*; reqwest fixes the
//! TLS posture when the `Client` is built and reuses one connection pool across
//! calls. So upstream's free `get_bytes(url, verify=...)` becomes
//! [`HttpClient::new`]`(verify)` + [`HttpClient::get_bytes`]`(url)`. The
//! `resolve_verify` precedence logic is preserved verbatim so callers pick the
//! posture the same way before constructing the client.
//!
//! `_parse_ssl_verify` (the config-string coercion) is **not** re-ported here:
//! it already lives in [`mtui_config::SslVerify`]. [`VerifyPolicy::from_config`]
//! bridges that typed config value into this layer's posture.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mtui_config::SslVerify;

use crate::error::{HttpError, Result};

/// Shared `(connect, read)` timeout for every outbound HTTP call, mirroring
/// upstream `HTTP_TIMEOUT = (5.0, 30.0)`. Bounds a stuck socket so a broken
/// network can't hang mtui indefinitely.
pub const HTTP_TIMEOUT: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(30));

/// The connect-phase component of [`HTTP_TIMEOUT`].
const CONNECT_TIMEOUT: Duration = HTTP_TIMEOUT.0;
/// The read-phase component of [`HTTP_TIMEOUT`].
const READ_TIMEOUT: Duration = HTTP_TIMEOUT.1;

/// Fallback locations of the distribution-managed CA bundle, probed only when
/// the interpreter/OpenSSL default (via `SSL_CERT_FILE`) names no existing file.
/// Mirrors upstream `_SYSTEM_CA_BUNDLES`.
const SYSTEM_CA_BUNDLES: &[&str] = &[
    "/etc/ssl/ca-bundle.pem",             // openSUSE / SLE
    "/etc/ssl/certs/ca-certificates.crt", // Debian / Ubuntu
    "/etc/pki/tls/certs/ca-bundle.crt",   // Fedora / RHEL
    "/etc/ssl/cert.pem",                  // Alpine, BSDs
];

/// Process-wide idempotency flag for the "verification disabled" warning,
/// mirroring upstream's `_warnings_disabled` guard around
/// `urllib3.disable_warnings`.
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);

/// The default connection-pool width, matching upstream `default_pool_size`
/// (`min(32, cpu + 4)`) so concurrent fan-out at a single host reuses
/// connections instead of churning the pool.
#[must_use]
pub fn default_pool_size() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    (cpus + 4).min(32)
}

/// A `reqwest`-compatible TLS verification posture: verify against the system
/// trust store (`Default(true)`), skip verification (`Default(false)`), or
/// verify against a specific CA bundle file (`CaBundle`). Mirrors upstream's
/// `VerifyPolicy = bool | str`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// Verify (`true`) or skip (`false`) using the default trust store.
    Default(bool),
    /// Verify against the CA bundle / certificate at this path.
    CaBundle(PathBuf),
}

impl VerifyPolicy {
    /// Whether this policy actually verifies certificates.
    ///
    /// `Default(false)` is the only non-verifying posture; a CA-bundle path is
    /// still "verifying", matching upstream where a truthy `verify` string
    /// keeps the insecure-request warning active.
    #[must_use]
    pub fn verifies(&self) -> bool {
        !matches!(self, Self::Default(false))
    }

    /// Bridge a typed [`mtui_config::SslVerify`] into this layer's posture.
    ///
    /// - [`SslVerify::Enabled`] → verify, preferring the system CA bundle
    ///   ([`system_ca_bundle`]) when one is found (matching upstream's default,
    ///   which prefers the distribution bundle over certifi), else
    ///   `Default(true)`.
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

/// Pick the effective verify value for a request, mirroring upstream
/// `resolve_verify`.
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
/// Mirrors upstream `disable_insecure_warnings`: the first call warns
/// process-wide and later calls are cheap no-ops. Called only when
/// verification is deliberately disabled, so REPL output stays readable.
pub fn disable_insecure_warnings() {
    if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            "TLS certificate verification is disabled (ssl_verify = false); \
             connections are not authenticated"
        );
    }
}

/// A shared outbound HTTP client with a fixed timeout and TLS-verify posture.
///
/// Built once and reused so concurrent fan-out at a single host reuses pooled
/// connections. Replaces upstream's `build_session(verify)`.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Build a client with the given [`VerifyPolicy`].
    ///
    /// Applies [`HTTP_TIMEOUT`] (connect + read), sizes the per-host idle pool
    /// to [`default_pool_size`], and derives the TLS posture:
    ///
    /// - `Default(true)` → verify against the built-in trust store;
    /// - `Default(false)` → accept invalid certs (and warn once via
    ///   [`disable_insecure_warnings`]);
    /// - `CaBundle(path)` → verify against *only* that PEM bundle, matching
    ///   upstream's `session.verify = "/path"` (which replaces, not augments,
    ///   the trust store).
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub fn new(verify: VerifyPolicy) -> Result<Self> {
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

        Ok(Self {
            inner: builder.build()?,
        })
    }

    /// Borrow the underlying `reqwest::Client` for callers that need the full
    /// request surface (JSON, headers, POST) rather than a bare GET.
    #[must_use]
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }

    /// GET `url` and return the raw response body as bytes.
    ///
    /// The single GET-to-bytes path for callers that just want a payload (a log
    /// file, a YAML document). Raises for any non-2xx status, mirroring
    /// upstream `get_bytes` → `response.raise_for_status()`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Request`] on any transport failure or non-2xx HTTP
    /// status.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.inner.get(url).send().await?.error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }
}

/// Read a PEM CA bundle from `path` into reqwest certificates.
fn load_ca_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>> {
    let pem = std::fs::read(path).map_err(|source| HttpError::CaBundle {
        path: path.display().to_string(),
        source,
    })?;
    reqwest::Certificate::from_pem_bundle(&pem).map_err(HttpError::Request)
}

/// Return `true` if `err` is (or was caused by) a TLS certificate-verification
/// failure.
///
/// Mirrors upstream `is_ssl_verification_error`: reqwest wraps the underlying
/// rustls verification error several layers deep, so this walks the
/// [`std::error::Error::source`] chain (guarding against cycles) and falls back
/// to matching `CERTIFICATE_VERIFY_FAILED` / a certificate-verify phrase in the
/// stringified error for transports that only surface it in the message.
#[must_use]
pub fn is_ssl_verification_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut seen = 0usize;
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        // Guard against a self-referential source chain looping forever.
        if seen > 64 {
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
/// Ported from upstream `ssl_verification_hint`: it names the concrete remedies
/// instead of dumping a transport error. The custom-bundle example stays
/// generic on purpose — naming the system bundle would suggest a no-op, since it
/// is usually already the verify source that just failed.
#[must_use]
pub fn ssl_verification_hint(host: Option<&str>) -> String {
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

/// The system's CA bundle path, or `None` when none is found.
///
/// Ported from upstream `system_ca_bundle`: prefer the `SSL_CERT_FILE`
/// environment override (the interpreter's OpenSSL default cafile in the Python
/// version), then the well-known distribution paths in [`SYSTEM_CA_BUNDLES`].
/// Returns the first candidate that is an existing file.
#[must_use]
pub fn system_ca_bundle() -> Option<PathBuf> {
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
        // Cannot monkeypatch available_parallelism, but the invariant upstream
        // asserts (min(32, cpu + 4)) means the result is always in [5, 32].
        let n = default_pool_size();
        assert!((5..=32).contains(&n), "pool size {n} out of [5, 32]");
    }

    #[test]
    fn resolve_verify_precedence() {
        // (default, override, expected) mirroring upstream's parametrization.
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
    fn verify_policy_verifies_flag() {
        assert!(VerifyPolicy::Default(true).verifies());
        assert!(!VerifyPolicy::Default(false).verifies());
        assert!(VerifyPolicy::CaBundle(PathBuf::from("/x")).verifies());
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
        // (matching upstream's prefer-system default).
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
}
