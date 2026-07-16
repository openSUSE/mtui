//! HTTP transport for the native OBS/IBS API (native `reqwest`, no `osc`).
//!
//! Ported from upstream `mtui/data_sources/obs/client.py`. Mirrors the crate's
//! [`Gitea`](crate::gitea::Gitea) request wrapper: one shared
//! [`HttpClient`](crate::http::HttpClient) (built with a fixed timeout + TLS
//! posture) carries the handful of calls one QAM operation makes. SSH-signature
//! auth is injected through the [`ObsAuth`] seam so this transport foundation
//! (G1a) is testable now with [`NoAuth`]; the real signer lands in G1c.
//!
//! Two upstream behaviours are load-bearing and preserved:
//!
//! * **Never log the Authorization header or the request body.** Only the
//!   method + URL are logged, at `debug`.
//! * **A coarse between-calls time budget.** A whole operation makes several
//!   calls; the `[obs] request_timeout` deadline is checked *before* each one.
//!   There is no safe in-process mid-call hard kill, so the budget bounds the
//!   operation between hops rather than aborting a call in flight.

use std::sync::Arc;
use std::time::{Duration, Instant};

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::{Method, RequestBuilder};

use crate::http::{
    HttpClient, VerifyPolicy, is_ssl_verification_error, sanitize_url, ssl_verification_hint,
};
use crate::obs::errors::ObsError;

/// The SSH-signature auth seam for the OBS transport.
///
/// Upstream sends the first request unauthenticated and only signs on a `401`
/// `WWW-Authenticate: Signature` challenge, resending **exactly once** with the
/// `Authorization: Signature` header. reqwest has no `requests`-style response
/// hook, so the retry-once loop lives in `ObsClient::request`; this trait is
/// the seam it drives: [`authorization`](ObsAuth::authorization) builds the
/// header value for the challenge `realm`.
///
/// Implementations must **never** cause the auth material (header/signature) to
/// be logged — they only ever *return* it to the transport, which logs method +
/// URL only.
#[async_trait::async_trait]
pub trait ObsAuth: Send + Sync {
    /// Build the `Authorization: Signature …` value for the challenge `realm`.
    ///
    /// Returns `Ok(None)` when no auth is configured (the [`NoAuth`] stub), in
    /// which case the transport does **not** retry the `401`. Returns
    /// `Err(..)` for a resolvable-but-failed signer (fail-closed).
    async fn authorization(&self, realm: &str) -> Result<Option<String>, ObsError>;
}

/// A no-op [`ObsAuth`] that never signs.
///
/// Used by callers that hold a session cookie already, and by tests; a `401`
/// challenge is returned as-is (no retry) because there is nothing to sign.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAuth;

#[async_trait::async_trait]
impl ObsAuth for NoAuth {
    async fn authorization(&self, _realm: &str) -> Result<Option<String>, ObsError> {
        Ok(None)
    }
}

/// Extract the top-level `<status><summary>` text from an OBS error body.
///
/// Ported from upstream `_error_summary`. Best-effort: any parse failure or an
/// absent summary yields an empty string.
///
/// **Security (DTD/XXE guard):** OBS never sends a DTD, so a body carrying
/// `<!DOCTYPE` or `<!ENTITY` is refused *before* parsing — this neutralises an
/// entity-expansion DoS on a compromised/MITM'd error body. Defence in depth:
/// `quick-xml` does not expand general entities anyway (it surfaces them as
/// distinct events rather than inlining their replacement text), so even a
/// DTD-free body with an entity reference never expands.
#[must_use]
pub fn error_summary(body: &str) -> String {
    if body.contains("<!DOCTYPE") || body.contains("<!ENTITY") {
        return String::new();
    }

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut in_summary = false;
    let mut summary = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"summary" {
                    in_summary = true;
                }
            }
            Ok(Event::Text(e)) if in_summary => match e.decode() {
                Ok(text) => summary.push_str(text.as_ref()),
                Err(_) => return String::new(),
            },
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"summary" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return String::new(),
            _ => {}
        }
        buf.clear();
    }

    summary.trim().to_owned()
}

/// A thin OBS API client over one shared, authenticated HTTP transport.
///
/// Build once per operation (like upstream): the constructor fixes the API base
/// URL, the TLS posture, the auth signer, and the coarse time budget; each
/// [`get`](ObsClient::get) / [`post`](ObsClient::post) is one bounded hop.
#[derive(Clone)]
pub struct ObsClient {
    http: HttpClient,
    api_url: String,
    auth: Arc<dyn ObsAuth>,
    deadline: Instant,
}

impl ObsClient {
    /// Build a client for `api_url` with the given time budget, TLS posture and
    /// auth signer.
    ///
    /// Explicit parameters rather than a `Config`/oscrc coupling keep this
    /// transport foundation self-contained; wiring from `[obs]` config +
    /// resolved credentials lands in later subtasks. The trailing `/` is
    /// stripped from `api_url` (upstream `rstrip("/")`), and the coarse deadline
    /// is set to `now + request_timeout` (upstream `time.monotonic() +
    /// obs_request_timeout`).
    ///
    /// # Errors
    ///
    /// Returns [`ObsError::Http`] if the shared HTTP client cannot be built
    /// (e.g. a configured CA bundle cannot be read).
    pub fn new(
        api_url: &str,
        request_timeout: Duration,
        verify: VerifyPolicy,
        auth: Arc<dyn ObsAuth>,
    ) -> Result<Self, ObsError> {
        let http = HttpClient::new(verify)?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            auth,
            deadline: Instant::now() + request_timeout,
        })
    }

    /// Join `path` (with any query params) onto the API base, mirroring upstream
    /// `_url` + the params `requests` would have appended.
    ///
    /// The query is encoded manually because this workspace's minimal `reqwest`
    /// build does not expose `.query()` (see [`crate::teregen`] /
    /// [`crate::openqa`]).
    fn url(&self, path: &str, params: &[(&str, String)]) -> String {
        let mut url = format!("{}/{}", self.api_url, path.trim_start_matches('/'));
        let qs = build_query_string(params);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs);
        }
        url
    }

    /// Abort with [`ObsError::Timeout`] if the between-calls budget is spent.
    fn check_budget(&self, url: &str) -> Result<(), ObsError> {
        if Instant::now() > self.deadline {
            return Err(ObsError::Timeout(format!(
                "OBS operation exceeded its between-calls time budget before {url}"
            )));
        }
        Ok(())
    }

    /// Build a fresh request builder for `url` with the standard OBS headers and
    /// optional body. `auth` is deliberately *not* applied here — the first
    /// request goes out unauthenticated (upstream), and the retry attaches the
    /// signed header directly.
    fn builder(&self, method: &Method, url: &str, body: Option<&str>) -> RequestBuilder {
        let mut builder = self
            .http
            .inner()
            .request(method.clone(), url)
            .header("Accept", "application/xml");
        if let Some(body) = body {
            builder = builder
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body.as_bytes().to_vec());
        }
        builder
    }

    /// Send one prepared request, mapping transport failures to [`ObsError`].
    async fn send(
        &self,
        method: &Method,
        url: &str,
        builder: RequestBuilder,
    ) -> Result<reqwest::Response, ObsError> {
        builder.send().await.map_err(|e| {
            if is_ssl_verification_error(&e) {
                let host = host_of(url);
                tracing::error!("{}", ssl_verification_hint(host.as_deref()));
                tracing::debug!("OBS TLS error detail: {e}");
            } else {
                tracing::error!("OBS {method} {} failed: {e}", sanitize_url(url));
            }
            ObsError::Http(e.into())
        })
    }

    /// The shared request path for GET/POST, ported from upstream `_request`.
    ///
    /// Sends the first request unauthenticated; on a `401` that offers a
    /// `Signature` challenge, signs `(created)` over the challenge realm and
    /// resends **exactly once** with the `Authorization: Signature` header. The
    /// header is never logged — only method + URL are, at `debug`.
    async fn request(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<String, ObsError> {
        let url = self.url(path, params);
        self.check_budget(&url)?;

        tracing::debug!("OBS {method} {}", sanitize_url(&url));
        let response = self
            .send(&method, &url, self.builder(&method, &url, body))
            .await?;

        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.retry_signed(&method, &url, body, response).await?
        } else {
            response
        };

        self.finish(&method, &url, response).await
    }

    /// On a `401`, build the signed retry and resend once.
    ///
    /// Returns the retried response when the challenge offers `Signature` and a
    /// signer is configured; otherwise returns the original `401` unchanged (the
    /// caller then surfaces it as an [`ObsError::Api`]). A signer error
    /// (fail-closed) propagates.
    async fn retry_signed(
        &self,
        method: &Method,
        url: &str,
        body: Option<&str>,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, ObsError> {
        let schemes = crate::obs::auth::challenge_params(response.headers());
        let Some(params) = schemes.get("signature") else {
            tracing::error!(
                "OBS returned 401 but did not offer Signature auth (offered: {})",
                if schemes.is_empty() {
                    "nothing".to_owned()
                } else {
                    schemes.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            );
            return Ok(response);
        };
        let realm = params.get("realm").map_or("", String::as_str).to_owned();

        let Some(header) = self.auth.authorization(&realm).await? else {
            // No signer configured (NoAuth): return the original 401 unchanged.
            return Ok(response);
        };

        // Release the challenge response before resending.
        drop(response);

        let builder = self
            .builder(method, url, body)
            .header("Authorization", header);
        self.send(method, url, builder).await
    }

    /// Turn a final (non-retryable) response into its body or an [`ObsError`].
    async fn finish(
        &self,
        method: &Method,
        url: &str,
        response: reqwest::Response,
    ) -> Result<String, ObsError> {
        if response.status().is_success() {
            return response.text().await.map_err(|e| ObsError::Http(e.into()));
        }
        Err(self.api_error(method, url, response).await)
    }

    /// Build an [`ObsError::Api`] from a failing response (consumes its body).
    async fn api_error(&self, method: &Method, url: &str, response: reqwest::Response) -> ObsError {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let summary = error_summary(&text);
        let suffix = if summary.is_empty() {
            String::new()
        } else {
            format!(": {summary}")
        };
        let safe_url = sanitize_url(url);
        tracing::warn!("OBS {method} {safe_url} -> {status}{suffix}");
        ObsError::Api {
            status,
            url: safe_url,
            summary,
        }
    }

    /// GET `path` (relative to the API base) and return the response body.
    ///
    /// # Errors
    ///
    /// Returns [`ObsError::Timeout`] if the between-calls budget is spent,
    /// [`ObsError::Http`] on transport failure, or [`ObsError::Api`] on a
    /// non-2xx status.
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<String, ObsError> {
        self.request(Method::GET, path, params, None).await
    }

    /// POST `body` to `path` (relative to the API base) and return the response
    /// body.
    ///
    /// # Errors
    ///
    /// Returns [`ObsError::Timeout`] if the between-calls budget is spent,
    /// [`ObsError::Http`] on transport failure, or [`ObsError::Api`] on a
    /// non-2xx status.
    pub async fn post(
        &self,
        path: &str,
        params: &[(&str, String)],
        body: &str,
    ) -> Result<String, ObsError> {
        self.request(Method::POST, path, params, Some(body)).await
    }
}

/// Build the percent-encoded `key=value&...` query string.
///
/// Uses `application/x-www-form-urlencoded` encoding (space → `+`), matching
/// reqwest's `.query()` wire form, which this workspace's minimal reqwest build
/// does not expose (see [`crate::teregen`]).
fn build_query_string(params: &[(&str, String)]) -> String {
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

/// Extract the host (authority without any port) from an `scheme://host[:port]/…`
/// URL, for the TLS-failure hint. Returns `None` if the shape is unexpected.
///
/// Mirrors the same helper in [`crate::gitea`].
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from upstream tests/test_obs_client.py::test_error_summary.
    #[test]
    fn error_summary_extracts_trimmed_summary() {
        assert_eq!(
            error_summary("<status><summary> boom </summary></status>"),
            "boom"
        );
    }

    #[test]
    fn error_summary_empty_when_no_summary_element() {
        assert_eq!(error_summary("<status/>"), "");
    }

    #[test]
    fn error_summary_empty_for_non_xml() {
        assert_eq!(error_summary("not xml"), "");
    }

    #[test]
    fn error_summary_skips_dtd_bearing_body_without_expanding_entities() {
        // A DTD-bearing error body is refused before parsing, so the entity is
        // never expanded (no `boom`, empty string).
        let body = r#"<!DOCTYPE x [<!ENTITY e "boom">]><status><summary>&e;</summary></status>"#;
        assert_eq!(error_summary(body), "");
    }

    #[test]
    fn error_summary_does_not_expand_lone_entity_reference() {
        // Defence in depth: even without a DTD, an entity reference is not
        // inlined by the reader, so the summary stays empty rather than "boom".
        let body = "<status><summary>&e;</summary></status>";
        assert_eq!(error_summary(body), "");
    }

    #[test]
    fn host_of_extracts_authority_without_port() {
        assert_eq!(
            host_of("https://api.suse.de:443/request/1").as_deref(),
            Some("api.suse.de")
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[tokio::test]
    async fn no_auth_never_signs() {
        // The seam: NoAuth returns no header, so a 401 challenge is not retried.
        assert!(
            NoAuth
                .authorization("Use your developer account")
                .await
                .expect("NoAuth never errors")
                .is_none()
        );
    }
}
