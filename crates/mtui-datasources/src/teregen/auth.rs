//! SSH-signature bearer-token auth for teregen's `/api/v2` (Phase 2 of the
//! JSON-template migration).
//!
//! [`TeregenAuth`] drives the challenge/response against `POST
//! .../auth/ssh/challenge` and `POST .../auth/ssh/verify`, using
//! [`crate::sshsig::Signer`] (the same key-resolution engine the native OBS
//! backend uses) to sign the nonce. [`TeregenAuth::token`] is cached-or-mint:
//! it consults the on-disk [`TokenStore`] first and only calls the network when
//! there is no usable cached token.
//!
//! The wire contract (read from teregen's `Controller/Auth.pm`, `Auth.pm`,
//! `Model/Token.pm`):
//!
//! * `challenge` — body `{"username"}` → `200 {"nonce": "<64 lowercase hex>"}`,
//!   `400` invalid body, `429 {"error":"too_many_requests"}` past 20
//!   challenges/60s per username (an approximate, non-atomic limit).
//! * `verify` — body `{"username", "nonce", "signature"}` → `200 {"token"}`,
//!   `400`, `401 {"error":"unauthorized"}` for **every** failure reason —
//!   unknown user, no key, wrong key, bad signature, or a stale/consumed nonce
//!   — deliberately indistinguishable (a `DUMMY_KEY` keeps timing uniform).
//! * The signed message is the **bare nonce bytes, no trailing newline**; the
//!   signature is the **armored PEM** text (teregen's wire format, unlike
//!   OBS's raw base64).
//!
//! The nonce is shape-checked (`^[0-9a-f]{64}$`) **before** signing, so a
//! mangled challenge response fails without ever touching the agent. The
//! verify response body carries the token, so it is parsed and **never
//! logged** — not at `DEBUG`, not inside an error; [`TeregenAuth`] gets a
//! manual [`Debug`] (P2-D7).

use std::path::PathBuf;

use serde_json::{Value, json};
use thiserror::Error;

use crate::error::HttpError;
use crate::http::{HttpClient, MAX_API_BODY, read_body_capped, sanitize_url};
use crate::obs::auth::{AgentKeys, RusshAgent};
use crate::sshsig::{Signer, SshSigError};
use crate::teregen::tokens::{CachedToken, TokenStore};

/// The SSHSIG namespace teregen expects, unless overridden (e.g. the
/// `cargo xtask teregen-login --namespace` deliberate-negative probe).
pub const DEFAULT_NAMESPACE: &str = "teregen-auth";

/// The [`Signer`] context label for fail-closed hints.
const CONTEXT: &str = "teregen auth";

/// A hint naming every cause a `401` from `/auth/ssh/verify` can hide — the
/// server deliberately makes them indistinguishable.
const UNAUTHORIZED_HINT: &str = "the server does not distinguish these, but a 401 from \
     /auth/ssh/verify means one of: the principal is unknown to teregen's \
     sshkeys.yaml, that principal has no key configured, the signing key does \
     not match the configured one, the signature itself is invalid, or the \
     nonce was stale/already consumed (nonces are single-use and expire after \
     60s)";

/// Errors from teregen SSH-signature auth.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TeregenAuthError {
    /// The challenge endpoint is rate-limited (approximately 20/60s per
    /// username). Callers must not retry in a loop.
    #[error(
        "teregen auth challenge was rate-limited (approximately 20 \
         challenges/60s per user); wait and retry"
    )]
    RateLimited,

    /// `/auth/ssh/verify` returned `401` — every failure reason folds here.
    #[error("teregen auth was refused (401): {hint}")]
    Unauthorized {
        /// Names the indistinguishable possible causes.
        hint: String,
    },

    /// A non-401/429 protocol fault: a `400`, an unexpected status, or a
    /// malformed/missing-field JSON body.
    #[error("{0}")]
    Protocol(String),

    /// A transport failure or a non-2xx status the layer below already
    /// sanitizes.
    #[error(transparent)]
    Transport(#[from] HttpError),

    /// The nonce could not be signed: a key-resolution or SSHSIG failure.
    #[error(transparent)]
    Signing(#[from] SshSigError),

    /// The on-disk token store could not be read or written (surfaced only by
    /// [`TeregenAuth::invalidate`] — a mint's own cache-write failure is
    /// logged and does not fail the mint, since the token is already valid
    /// server-side).
    #[error("teregen token store error: {0}")]
    Store(String),
}

/// `true` when `s` is exactly 64 lowercase hex characters.
fn is_valid_nonce(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The current instant as an RFC3339 UTC timestamp, informational only (see
/// [`CachedToken::minted_at`]).
///
/// The workspace pins `chrono` without the `clock` feature (single-static-
/// binary contract), so "now" comes from [`std::time::SystemTime`] rather than
/// `chrono::Utc::now()` — the same pattern as `oqa_search::current_utc_date`.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

/// SSH-signature bearer-token auth for teregen's `/api/v2`.
///
/// Exactly one of `sshkey_path`/`sshkey_fingerprint` (via [`Signer`])
/// identifies the signing key — the pair produced by
/// [`crate::obs::oscrc::read_credentials`] (P2-D1: teregen reuses the oscrc
/// identity, no new config key).
pub struct TeregenAuth<A: AgentKeys = RusshAgent> {
    base: String,
    principal: String,
    namespace: String,
    signer: Signer<A>,
    store: Option<TokenStore>,
    http: HttpClient,
}

impl<A: AgentKeys> std::fmt::Debug for TeregenAuth<A> {
    /// Manual impl (P2-D7): the struct holds no token field (one is fetched
    /// per call, never cached in memory), but a derive would print the
    /// `HttpClient`/store internals for no reader benefit.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeregenAuth")
            .field("base", &self.base)
            .field("principal", &self.principal)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl TeregenAuth<RusshAgent> {
    /// Build against the real ssh-agent (`$SSH_AUTH_SOCK`) for agent-backed
    /// keys, caching to the XDG data dir ([`TokenStore::new`]).
    #[must_use]
    pub fn new(
        base: impl Into<String>,
        principal: impl Into<String>,
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self::with_agent(
            base,
            principal,
            sshkey_path,
            sshkey_fingerprint,
            http,
            RusshAgent::default(),
        )
    }
}

impl<A: AgentKeys> TeregenAuth<A> {
    /// Build with an explicit [`AgentKeys`] backend (used by tests).
    pub fn with_agent(
        base: impl Into<String>,
        principal: impl Into<String>,
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
        http: HttpClient,
        agent: A,
    ) -> Self {
        Self {
            base: base.into(),
            principal: principal.into(),
            namespace: DEFAULT_NAMESPACE.to_owned(),
            signer: Signer::with_agent(sshkey_path, sshkey_fingerprint, CONTEXT, agent),
            store: TokenStore::new(),
            http,
        }
    }

    /// Override the SSHSIG namespace (default [`DEFAULT_NAMESPACE`]) — the
    /// `cargo xtask teregen-login --namespace` deliberate-negative probe uses
    /// this to prove a wrong namespace is rejected.
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Override the token store (`None` disables caching — `--no-store`).
    #[must_use]
    pub fn with_store(mut self, store: Option<TokenStore>) -> Self {
        self.store = store;
        self
    }

    /// The configured principal (for callers that want to display it).
    #[must_use]
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// `POST {base}/auth/ssh/challenge`: request a fresh nonce.
    ///
    /// # Errors
    ///
    /// [`TeregenAuthError::RateLimited`] on `429`, [`TeregenAuthError::Protocol`]
    /// on `400`/an unexpected status/a malformed body, or
    /// [`TeregenAuthError::Transport`] on a transport failure.
    pub async fn challenge(&self) -> Result<String, TeregenAuthError> {
        let url = format!("{}/auth/ssh/challenge", self.base);
        tracing::debug!(
            "teregen auth: requesting a challenge at {}",
            sanitize_url(&url)
        );
        let body = json!({ "username": self.principal });
        let response = self
            .http
            .inner()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(HttpError::from)?;

        match response.status().as_u16() {
            200 => {
                let bytes = read_body_capped(response, MAX_API_BODY).await?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                    TeregenAuthError::Protocol(format!("challenge response is not JSON: {e}"))
                })?;
                value
                    .get("nonce")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        TeregenAuthError::Protocol(
                            "challenge response is missing 'nonce'".to_owned(),
                        )
                    })
            }
            429 => Err(TeregenAuthError::RateLimited),
            400 => Err(TeregenAuthError::Protocol(
                "teregen auth challenge request was refused (400)".to_owned(),
            )),
            other => Err(TeregenAuthError::Protocol(format!(
                "teregen auth challenge returned unexpected status {other}"
            ))),
        }
    }

    /// `POST {base}/auth/ssh/verify`: sign `nonce` and exchange it for a token.
    ///
    /// The nonce is shape-checked before any signing happens, so a malformed
    /// challenge response never reaches the agent or the wire.
    ///
    /// # Errors
    ///
    /// [`TeregenAuthError::Protocol`] for a malformed nonce, a `400`, an
    /// unexpected status, or a missing/empty token in a `200` body;
    /// [`TeregenAuthError::Unauthorized`] on `401`;
    /// [`TeregenAuthError::Signing`] if the nonce cannot be signed;
    /// [`TeregenAuthError::Transport`] on a transport failure.
    pub async fn verify(&self, nonce: &str) -> Result<String, TeregenAuthError> {
        if !is_valid_nonce(nonce) {
            return Err(TeregenAuthError::Protocol(
                "teregen auth challenge returned a malformed nonce (expected 64 lowercase hex \
                 characters)"
                    .to_owned(),
            ));
        }

        let sig = self.signer.sign(&self.namespace, nonce.as_bytes()).await?;
        let armored = crate::sshsig::encode_pem(&sig)?;

        let url = format!("{}/auth/ssh/verify", self.base);
        tracing::debug!("teregen auth: verifying at {}", sanitize_url(&url));
        let body = json!({
            "username": self.principal,
            "nonce": nonce,
            "signature": armored,
        });
        let response = self
            .http
            .inner()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(HttpError::from)?;

        match response.status().as_u16() {
            200 => {
                let bytes = read_body_capped(response, MAX_API_BODY).await?;
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                    TeregenAuthError::Protocol(format!("verify response is not JSON: {e}"))
                })?;
                value
                    .get("token")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        TeregenAuthError::Protocol(
                            "verify response is missing a non-empty 'token'".to_owned(),
                        )
                    })
            }
            401 => Err(TeregenAuthError::Unauthorized {
                hint: UNAUTHORIZED_HINT.to_owned(),
            }),
            400 => Err(TeregenAuthError::Protocol(
                "teregen auth verify request was refused (400)".to_owned(),
            )),
            other => Err(TeregenAuthError::Protocol(format!(
                "teregen auth verify returned unexpected status {other}"
            ))),
        }
    }

    /// The cached-or-mint entry point: return a usable bearer token.
    ///
    /// Consults the token store first (zero HTTP on a cache hit); on a miss,
    /// mints via [`challenge`](Self::challenge) + [`verify`](Self::verify) and
    /// caches the result. A cache-write failure is logged and does not fail
    /// the mint — the token is already valid server-side.
    ///
    /// # Errors
    ///
    /// See [`challenge`](Self::challenge) / [`verify`](Self::verify).
    pub async fn token(&self) -> Result<String, TeregenAuthError> {
        if let Some(store) = &self.store
            && let Some(cached) = store.load(&self.base, &self.principal)
        {
            return Ok(cached.token);
        }
        self.mint().await
    }

    /// Unconditionally challenge + verify + cache a fresh token.
    async fn mint(&self) -> Result<String, TeregenAuthError> {
        let nonce = self.challenge().await?;
        let token = self.verify(&nonce).await?;
        if let Some(store) = &self.store {
            let cached = CachedToken {
                base: self.base.clone(),
                principal: self.principal.clone(),
                token: token.clone(),
                minted_at: now_rfc3339(),
            };
            if let Err(e) = store.store(&cached) {
                tracing::warn!(
                    "could not cache the freshly minted teregen token at {}: {e}",
                    store.path().display()
                );
            }
        }
        Ok(token)
    }

    /// Drop any cached token, forcing the next [`token`](Self::token) call to
    /// mint (P2-D6: used on a `401` from an authenticated request).
    ///
    /// A missing file is not an error (there was nothing to invalidate).
    ///
    /// # Errors
    ///
    /// [`TeregenAuthError::Store`] if the cache file exists but could not be
    /// removed.
    pub fn invalidate(&self) -> Result<(), TeregenAuthError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        match std::fs::remove_file(store.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(TeregenAuthError::Store(format!(
                "could not remove cached token at {}: {e}",
                store.path().display()
            ))),
        }
    }

    /// Send `method url` with `Authorization: Bearer <token>` attached.
    ///
    /// On a `401`, [`invalidate`](Self::invalidate)s the cached token, mints
    /// once, and retries **once** more. A second `401` (or any other error) is
    /// surfaced without a further attempt — this mirrors the OBS client's
    /// "resent exactly once" rule, and matters because the challenge endpoint
    /// is rate-limited and a mint costs an ssh-agent round trip, so a retry
    /// loop against a genuinely unauthorised principal would burn the rate
    /// budget in under a second (P2-D6). Phase 3/5's v2 client is the intended
    /// caller; this is the one place that dance lives.
    ///
    /// # Errors
    ///
    /// See [`token`](Self::token); a transport failure on either attempt
    /// surfaces as [`TeregenAuthError::Transport`].
    pub async fn authenticated_request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::Response, TeregenAuthError> {
        self.authenticated_request_with(method, url, |b| b).await
    }

    /// Like [`authenticated_request`](Self::authenticated_request), but
    /// `customize` runs on the request builder before it is sent — e.g. to
    /// attach a body and extra headers for a `PUT` (Phase 5's write path).
    ///
    /// `customize` is `Fn`, not `FnOnce`: it is applied identically on the
    /// first attempt and again on the re-mint resend, so a `PUT`'s body and
    /// headers are not silently dropped from the retry.
    ///
    /// Retrying a write this way is safe *only* because a `401` is refused by
    /// the server before any write happens (`require_auth` is the first check
    /// in teregen's `upload`) — a re-sent `PUT` on the retry path can only
    /// ever be the *first* write to actually land, never a second one.
    ///
    /// # Errors
    ///
    /// See [`authenticated_request`](Self::authenticated_request).
    pub async fn authenticated_request_with<F>(
        &self,
        method: reqwest::Method,
        url: &str,
        customize: F,
    ) -> Result<reqwest::Response, TeregenAuthError>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        let token = self.token().await?;
        let response = self
            .bearer_request(method.clone(), url, &token, &customize)
            .await?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }

        self.invalidate()?;
        let token = self.token().await?;
        let response = self.bearer_request(method, url, &token, &customize).await?;
        if response.status().as_u16() == 401 {
            return Err(TeregenAuthError::Unauthorized {
                hint: UNAUTHORIZED_HINT.to_owned(),
            });
        }
        Ok(response)
    }

    /// Send one bearer-authenticated request, with no retry logic of its own.
    async fn bearer_request<F>(
        &self,
        method: reqwest::Method,
        url: &str,
        token: &str,
        customize: &F,
    ) -> Result<reqwest::Response, TeregenAuthError>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        tracing::debug!("teregen: {method} {}", sanitize_url(url));
        let builder = self.http.inner().request(method, url).bearer_auth(token);
        Ok(customize(builder).send().await.map_err(HttpError::from)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_nonce_shapes() {
        assert!(is_valid_nonce(&"a".repeat(64)));
        assert!(is_valid_nonce(&"0123456789abcdef".repeat(4)));
    }

    #[test]
    fn invalid_nonce_shapes() {
        assert!(!is_valid_nonce(""));
        assert!(!is_valid_nonce(&"a".repeat(63)));
        assert!(!is_valid_nonce(&"a".repeat(65)));
        assert!(!is_valid_nonce(&"A".repeat(64)), "uppercase is rejected");
        assert!(!is_valid_nonce(&"g".repeat(64)), "non-hex is rejected");
    }

    /// `authenticated_request_with`'s `customize` closure must be applied on
    /// **both** the first attempt and the re-mint resend: dropping it on
    /// either turns this red (the retried request would carry a different
    /// header value, or none).
    #[tokio::test]
    async fn authenticated_request_with_applies_customize_on_both_attempts() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let store_file = dir.path().join("teregen-token.json");
        let key =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obs/id_ed25519");
        Mock::given(method("POST"))
            .and(path("/auth/ssh/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nonce": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "a".repeat(64),
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/thing"))
            .and(header("x-custom", "marker"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/thing"))
            .and(header("x-custom", "marker"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let auth = TeregenAuth::new(
            server.uri(),
            "alice".to_owned(),
            Some(key),
            None,
            HttpClient::new(crate::http::VerifyPolicy::Default(true)).unwrap(),
        )
        .with_store(Some(TokenStore::at(store_file)));
        let url = format!("{}/thing", server.uri());
        let response = auth
            .authenticated_request_with(reqwest::Method::PUT, &url, |b| {
                b.header("x-custom", "marker").body("payload")
            })
            .await
            .expect("succeeds after one re-mint, header intact on the retry");
        assert_eq!(response.status(), 200);

        let requests = server.received_requests().await.unwrap();
        let put_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/thing")
            .collect();
        assert_eq!(put_requests.len(), 2, "one 401 + one retry");
        for r in &put_requests {
            assert_eq!(r.headers.get("x-custom").unwrap(), "marker");
            assert_eq!(r.body, b"payload");
        }
    }

    #[test]
    fn unauthorized_hint_names_every_cause() {
        for cause in [
            "unknown to teregen",
            "no key configured",
            "does not match",
            "signature itself is invalid",
            "stale/already consumed",
        ] {
            assert!(
                UNAUTHORIZED_HINT.contains(cause),
                "hint missing {cause:?}: {UNAUTHORIZED_HINT}"
            );
        }
    }
}
