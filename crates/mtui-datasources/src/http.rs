//! Single source of truth for outbound HTTP timeout and TLS policy.
//!
//! Every HTTP(S) caller in mtui shares one `(connect, read)` timeout and one
//! TLS-verification policy instead of each deciding separately:
//!
//! - `HTTP_TIMEOUT` bounds a stuck socket so a broken network cannot hang mtui.
//! - [`VerifyPolicy::from_config`] bridges the typed
//!   [`mtui_config::SslVerify`] config value into the effective
//!   [`VerifyPolicy`]; verification is **on by default**.
//! - [`HttpClient`] builds one `reqwest::Client` with that fixed posture;
//!   reqwest fixes TLS and owns its pool at build time, so a caller resolves the
//!   policy first, then reuses the client.
//! - `is_ssl_verification_error` / `ssl_verification_hint` / `ssl_error_detail`
//!   give a short, actionable message for internal-CA hosts instead of a raw
//!   transport error.
//! - URL redaction has three legs, most load-bearing first: the strip at the
//!   [`HttpError`] conversion boundary, which most call sites (teregen, the QEM
//!   dashboard) rely on alone; `sanitize_url`, for interpolating a request URL
//!   into a message as context; and `root_cause`, reaching past an error layer
//!   whose own `Display` may embed a URL.
//! - Every body is read through `read_body_capped` — [`MAX_API_BODY`] for small
//!   JSON/text, `MAX_DOWNLOAD_BODY` for bulk downloads — so a hostile datasource
//!   cannot OOM mtui. Overflow surfaces as [`HttpError::BodyTooLarge`], which
//!   carries no URL so it cannot leak a credential embedded in a datasource URL.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mtui_config::{Config, SslVerify};
pub use mtui_types::url_redaction::sanitize_url;

use crate::error::{HttpError, Result};

/// Shared `(connect, read)` timeout for every outbound HTTP call: 5s to
/// connect, 30s to read.
pub(crate) const HTTP_TIMEOUT: (Duration, Duration) =
    (Duration::from_secs(5), Duration::from_secs(30));

/// The read timeout of the [`HttpClient::patient`] client, for an upstream that
/// answers only after a slow lookup of its own (TeReGen's SMELT-backed reads).
pub(crate) const PATIENT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The connect-phase component of [`HTTP_TIMEOUT`].
const CONNECT_TIMEOUT: Duration = HTTP_TIMEOUT.0;
/// The read-phase component of [`HTTP_TIMEOUT`].
const READ_TIMEOUT: Duration = HTTP_TIMEOUT.1;

/// Maximum body size for a JSON/text API response: 16 MiB is orders of
/// magnitude above any legitimate payload while still bounding a hostile server.
pub const MAX_API_BODY: usize = 16 * 1024 * 1024;

/// Maximum body size for an openQA API response: the `job_groups` list carries
/// every group's YAML template and is about 19 MB on openqa.suse.de.
pub(crate) const MAX_OPENQA_BODY: usize = 64 * 1024 * 1024;

/// Maximum body size for a bulk *download* (a `refhosts.yml` database or an
/// openQA log fetched via [`HttpClient::get_bytes`]): a generous, hard ceiling.
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
    /// [`SslVerify::Enabled`] prefers the distribution CA bundle
    /// (`system_ca_bundle`) over `Default(true)` when one is found.
    #[must_use]
    pub fn from_config(verify: &SslVerify) -> Self {
        Self::from_config_with_bundle(verify, system_ca_bundle())
    }

    /// Pure core of [`from_config`](Self::from_config); testable without the
    /// process-global `SSL_CERT_FILE`.
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

/// Percent-encode `params` into a `key=value&...` query string, without a
/// leading `?`: `application/x-www-form-urlencoded` (space → `+`), matching
/// reqwest's `.query()` wire form, which this build omits.
pub(crate) fn query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                urlencoding::encode(k),
                urlencoding::encode(v).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `base/path` (leading slashes of `path` dropped), followed by `?query` when
/// `query` is non-empty.
pub(crate) fn join_url(base: &str, path: &str, query: &str) -> String {
    let mut url = format!("{base}/{}", path.trim_start_matches('/'));
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    url
}

/// Warn once, process-wide, that verification is deliberately disabled.
fn disable_insecure_warnings() {
    if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            "TLS certificate verification is disabled (ssl_verify = false); \
             connections are not authenticated"
        );
    }
}

/// The timeout + TLS-verify posture shared by [`HttpClient::new`] and
/// [`HttpClient::openqa_transport`].
fn base_builder(verify: VerifyPolicy, read_timeout: Duration) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
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
    verify: VerifyPolicy,
    patient_read_timeout: Duration,
    patient: Arc<OnceLock<reqwest::Client>>,
    unredirected: Arc<OnceLock<reqwest::Client>>,
}

impl HttpClient {
    /// Build a client with the given [`VerifyPolicy`]. `CaBundle(path)` verifies
    /// against *only* that PEM bundle, replacing rather than augmenting the
    /// trust store.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub fn new(verify: VerifyPolicy) -> Result<Self> {
        Self::with_read_timeouts(verify, READ_TIMEOUT, PATIENT_READ_TIMEOUT)
    }

    /// Build a client with the TLS posture of `config.ssl_verify`.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::new(VerifyPolicy::from_config(&config.ssl_verify))
    }

    pub(crate) fn with_read_timeouts(
        verify: VerifyPolicy,
        read_timeout: Duration,
        patient_read_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            inner: base_builder(verify.clone(), read_timeout)?.build()?,
            verify,
            patient_read_timeout,
            patient: Arc::new(OnceLock::new()),
            unredirected: Arc::new(OnceLock::new()),
        })
    }

    /// The same client with a longer read timeout, for an upstream that answers
    /// only after a lookup of its own: the shared client's 30 s read timeout
    /// fires first whatever a request's own `timeout` says. Built on first use
    /// and shared by every clone.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub(crate) fn patient(&self) -> Result<&reqwest::Client> {
        if let Some(client) = self.patient.get() {
            return Ok(client);
        }
        let built = base_builder(self.verify.clone(), self.patient_read_timeout)?.build()?;
        Ok(self.patient.get_or_init(|| built))
    }

    /// The same posture with automatic redirects off, for a caller that must
    /// vet every hop itself. Built on first use and shared by every clone.
    ///
    /// # Errors
    ///
    /// As [`patient`](Self::patient).
    pub(crate) fn unredirected(&self) -> Result<&reqwest::Client> {
        if let Some(client) = self.unredirected.get() {
            return Ok(client);
        }
        let built = base_builder(self.verify.clone(), READ_TIMEOUT)?
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(self.unredirected.get_or_init(|| built))
    }

    /// Builds a dedicated `reqwest::Client` for injection into
    /// [`ruoqa::ClientBuilder::http_client`](https://docs.rs/ruoqa).
    ///
    /// Shares this crate's timeout/TLS posture but **not** its pool, and
    /// disables redirects and reqwest-level retries, which `ruoqa` requires of an
    /// injected client: it re-signs every request and follows same-origin
    /// redirects itself, so a reqwest redirect would forward the
    /// `X-API-Key`/`X-API-Hash` headers cross-origin and a retry would replay a
    /// stale signature.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::CaBundle`] if a configured CA bundle cannot be read
    /// or parsed, or [`HttpError::Request`] if the client fails to build.
    pub fn openqa_transport(verify: VerifyPolicy) -> Result<reqwest::Client> {
        Ok(base_builder(verify, READ_TIMEOUT)?
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
    /// Use [`get_bytes_capped`](Self::get_bytes_capped) with [`MAX_API_BODY`] for
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
    /// larger than `max` (see `read_body_capped`).
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Request`] on any transport failure or non-2xx HTTP
    /// status, or [`HttpError::BodyTooLarge`] if the body exceeds `max`.
    pub async fn get_bytes_capped(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        let response = self.inner.get(url).send().await?.error_for_status()?;
        read_body_capped(response, max).await
    }

    /// GET `url` and parse the body, capped at [`MAX_API_BODY`], as JSON.
    ///
    /// # Errors
    ///
    /// As [`get_bytes_capped`](Self::get_bytes_capped), plus
    /// [`HttpError::Json`] for a body that is not valid JSON.
    pub(crate) async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let bytes = self.get_bytes_capped(url, MAX_API_BODY).await?;
        serde_json::from_slice(&bytes).map_err(HttpError::Json)
    }
}

/// Read `response`'s body into a `Vec`, rejecting any body larger than `max`.
///
/// Fail-fast on the advertised `Content-Length`, then enforce the same ceiling
/// while streaming, so a lying length or an endless chunked body cannot exceed
/// `max` bytes buffered. Dropping `response` on rejection closes the connection.
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

/// Read at most `limit` bytes of `response`'s body, dropping the rest.
///
/// Unlike [`read_body_capped`] an overlong body is truncated, not an error;
/// dropping `response` closes the connection instead of draining the overflow.
pub(crate) async fn read_body_prefix(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < limit
        && let Some(chunk) = response.chunk().await?
    {
        let room = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(body)
}

/// Read a PEM CA bundle from `path` into reqwest certificates.
fn load_ca_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>> {
    let pem = std::fs::read(path).map_err(|source| HttpError::CaBundle {
        path: path.display().to_string(),
        source,
    })?;
    // `HttpError::from`, never the bare variant: the URL strip lives there.
    reqwest::Certificate::from_pem_bundle(&pem).map_err(HttpError::from)
}

/// The bound both source-chain walks below use against a self-referential chain.
const MAX_ERROR_SOURCE_LINKS: usize = 64;

/// Return `true` if `err` is (or was caused by) a TLS certificate-verification
/// failure.
///
/// reqwest wraps the rustls verification error several layers deep, so this
/// walks the [`std::error::Error::source`] chain, falling back to matching a
/// certificate-verify phrase in the stringified error.
#[must_use]
pub(crate) fn is_ssl_verification_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut seen = 0usize;
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
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
/// The custom-bundle example stays generic on purpose: the system bundle is
/// usually the verify source that just failed.
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

/// The DEBUG-level detail line accompanying [`ssl_verification_hint`]: the
/// deepest cause of `e`, or `e`'s own `Display` when it has no source.
///
/// Taking [`HttpError`] rather than `&dyn Error` makes "already crossed the
/// conversion boundary" a compile-time precondition, so neither branch can
/// render a URL. The walk reaches past reqwest's kind text to the actionable
/// rustls/IO cause.
#[must_use]
pub(crate) fn ssl_error_detail(e: &HttpError) -> String {
    root_cause(e).unwrap_or_else(|| e.to_string())
}

/// Convert a failed send into an [`HttpError`], logging it first.
///
/// A TLS verification failure logs the actionable hint (at `error`, naming
/// `host` when known) and the root cause at `debug`; anything else is left to
/// `plain`, which owns the message and level because they differ per service.
/// The raw `reqwest::Error` is never rendered: its `Display` appends the
/// request URL.
pub(crate) fn log_send_error(
    service: &str,
    host: Option<&str>,
    e: reqwest::Error,
    plain: impl FnOnce(&HttpError),
) -> HttpError {
    let ssl = is_ssl_verification_error(&e);
    let e = HttpError::from(e);
    if ssl {
        tracing::error!("{}", ssl_verification_hint(host));
        tracing::debug!("{service} TLS error detail: {}", ssl_error_detail(&e));
    } else {
        plain(&e);
    }
    e
}

/// Walks `e`'s source chain to the deepest cause, deliberately skipping `e`
/// itself (whose `Display` may embed a URL — reqwest's appends the request URL,
/// `ruoqa::Error::Connection`'s renders a redacted one), and returns it through
/// [`sanitize_url`] as a backstop.
///
/// That backstop handles a **single** embedded URL only: `sanitize_url` parses
/// one `scheme://[userinfo@]host` and silently no-ops on a second one in the
/// same string. It is a safety net, not a substitute for stripping at the
/// boundary.
///
/// Returns `None` if `e` carries no source, so callers fall back to the outer
/// error's own `Display`. A self-referential chain is bounded by
/// [`MAX_ERROR_SOURCE_LINKS`].
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

/// The system's CA bundle path, or `None`: the first existing file among the
/// `SSL_CERT_FILE` override and the distribution paths in [`SYSTEM_CA_BUNDLES`].
#[must_use]
fn system_ca_bundle() -> Option<PathBuf> {
    let env_cafile = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    resolve_ca_bundle(env_cafile, SYSTEM_CA_BUNDLES)
}

/// Pure core of [`system_ca_bundle`]; testable without mutating the
/// process-global `SSL_CERT_FILE` (unsound under parallel tests in edition 2024).
fn resolve_ca_bundle(env_cafile: Option<PathBuf>, fallbacks: &[&str]) -> Option<PathBuf> {
    let candidates = env_cafile
        .into_iter()
        .chain(fallbacks.iter().map(PathBuf::from));
    candidates.into_iter().find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset the process-wide insecure-warning flag so the idempotency
    /// assertions are order-independent.
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
        // available_parallelism can't be mocked; min(32, cpu + 4) is in [5, 32].
        let n = default_pool_size();
        assert!((5..=32).contains(&n), "pool size {n} out of [5, 32]");
    }

    #[test]
    fn query_string_encodes_spaces_as_plus() {
        let params = [("q", "a b&c".to_owned()), ("k y", "é".to_owned())];
        assert_eq!(query_string(&params), "q=a+b%26c&k%20y=%C3%A9");
        assert_eq!(query_string(&[]), "");
    }

    #[test]
    fn join_url_trims_leading_slashes_and_omits_an_empty_query() {
        assert_eq!(join_url("https://h/api", "/x/y", ""), "https://h/api/x/y");
        assert_eq!(join_url("https://h/api", "x", "a=1"), "https://h/api/x?a=1");
    }

    #[test]
    fn client_from_config_honours_ssl_verify() {
        let mut config = Config::default();
        config.ssl_verify = SslVerify::CaBundle("/nonexistent/mtui-ca.pem".into());
        assert!(matches!(
            HttpClient::from_config(&config),
            Err(HttpError::CaBundle { .. })
        ));
        assert!(HttpClient::from_config(&Config::default()).is_ok());
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
        let ca = PathBuf::from("/sys/ca.pem");
        assert_eq!(
            VerifyPolicy::from_config_with_bundle(&SslVerify::Enabled, Some(ca.clone())),
            VerifyPolicy::CaBundle(ca)
        );
    }

    #[test]
    fn from_config_enabled_falls_back_to_default_true_without_bundle() {
        assert_eq!(
            VerifyPolicy::from_config_with_bundle(&SslVerify::Enabled, None),
            VerifyPolicy::Default(true)
        );
    }

    #[test]
    fn resolve_ca_bundle_prefers_env_cafile() {
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
        // Assert the flag transition rather than capturing the log line.
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
    fn sanitize_url_reexport_redacts_userinfo() {
        assert_eq!(
            sanitize_url("https://alice:s3cret@host.example/api/v1?q=1"),
            "https://***@host.example/api/v1?q=1"
        );
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

    /// Three-deep chain: `root_cause` must return the *deepest* message.
    /// Stopping at the first `source()` instead of walking on turns this red.
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

    /// Driven by a *real* `HttpError`, because the property is one of the real
    /// type: `Request` keeps reqwest as its `#[source]`, so the walk reaches
    /// past reqwest's own `Display` — the layer carrying the URL — to the
    /// transport cause. Nothing listens on the loopback discard port (RFC 863).
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
        // The recovered cause is the actionable half; `e`'s own Display
        // carries the kind prefix, the walk returns the bare cause.
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

    #[test]
    fn new_gives_the_patient_client_the_patient_read_timeout() {
        let client = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        assert_eq!(client.patient_read_timeout, PATIENT_READ_TIMEOUT);
    }

    /// The patient client is built with its own read timeout, not the shared
    /// one: here the patient budget is the *shorter*, so only it can time out.
    #[tokio::test]
    async fn patient_client_uses_the_patient_read_timeout() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(600)))
            .mount(&server)
            .await;
        let client = HttpClient::with_read_timeouts(
            VerifyPolicy::Default(true),
            Duration::from_secs(5),
            Duration::from_millis(100),
        )
        .unwrap();
        let err = client
            .patient()
            .unwrap()
            .get(server.uri())
            .send()
            .await
            .unwrap_err();
        assert!(err.is_timeout(), "{err:?}");
    }

    /// [`HttpClient::openqa_transport`] must not follow a redirect: it would
    /// forward `X-API-Key`/`X-API-Hash` to whatever `Location` says, off-origin
    /// — reqwest strips only `Authorization`/`Cookie`/`Proxy-Authorization` on
    /// a cross-origin hop. `ruoqa` follows same-origin redirects itself, so the
    /// injected client must be a dead end at the first hop.
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

    /// `HttpError::status()` must surface the real non-2xx status from a
    /// live response, not just from a hand-built error — this is the seam
    /// `xtask corpus-survey` depends on to tell 404 apart from 503.
    #[tokio::test]
    async fn get_json_parses_and_names_a_malformed_body_verbatim() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"a":1}"#))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let client = HttpClient::new(VerifyPolicy::Default(true)).unwrap();

        let ok = client.get_json(&format!("{}/ok", server.uri())).await;
        assert_eq!(ok.unwrap(), serde_json::json!({"a": 1}));
        let bad = client
            .get_json(&format!("{}/bad", server.uri()))
            .await
            .unwrap_err();
        let expected = serde_json::from_str::<serde_json::Value>("not json")
            .unwrap_err()
            .to_string();
        assert!(matches!(bad, HttpError::Json(_)), "{bad:?}");
        assert_eq!(bad.to_string(), expected);
    }

    #[tokio::test]
    async fn get_bytes_capped_error_status_matches_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/generating"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = HttpClient::new(VerifyPolicy::Default(true)).unwrap();

        let not_found = client
            .get_bytes_capped(&format!("{}/missing", server.uri()), MAX_API_BODY)
            .await
            .unwrap_err();
        assert_eq!(not_found.status(), Some(404));

        let generating = client
            .get_bytes_capped(&format!("{}/generating", server.uri()), MAX_API_BODY)
            .await
            .unwrap_err();
        assert_eq!(generating.status(), Some(503));
    }

    /// A body-size rejection never reached a real status line, so it must not
    /// masquerade as one.
    #[test]
    fn body_too_large_has_no_status() {
        let err = HttpError::BodyTooLarge {
            limit: 1,
            seen: Some(2),
        };
        assert_eq!(err.status(), None);
    }
}
