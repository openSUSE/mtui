//! SSH-signature bearer-token auth for teregen's `/api/v2`.
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
//!   `400`, `401 {"error":"unauthorized","hint":…}`. The `hint` is one of a few
//!   safe causes ([`AuthHint`]): a stale/consumed nonce, a key-list outage, or
//!   `signature_rejected` for unknown user, no usable key, wrong key and bad
//!   signature alike (a `DUMMY_KEY` keeps timing uniform). A body without a
//!   known hint gets the generic list of causes.
//! * The signed message is the **bare nonce bytes, no trailing newline**; the
//!   signature is the **armored PEM** text (teregen's wire format, unlike
//!   OBS's raw base64).
//!
//! The nonce is shape-checked (`^[0-9a-f]{64}$`) **before** signing, so a
//! mangled challenge response fails without touching the agent. The verify
//! response body carries the token, so it is parsed and **never logged** — not
//! at `DEBUG`, not inside an error; [`TeregenAuth`] gets a manual [`Debug`].

use std::path::PathBuf;

use serde_json::{Value, json};
use thiserror::Error;

use crate::clock::now_rfc3339;
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

/// A hint naming every cause a `401` from `/auth/ssh/verify` can hide, for a
/// refusal that carries no (known) [`AuthHint`].
const UNAUTHORIZED_HINT: &str = "the server does not distinguish these, but a 401 from \
     /auth/ssh/verify means one of: the principal is unknown to teregen's \
     sshkeys.yaml, that principal has no key configured, the signing key does \
     not match the configured one, the signature itself is invalid, or the \
     nonce was stale/already consumed (nonces are single-use and expire after \
     60s)";

/// A hint for a `401` on the write itself after a fresh mint, so none of the
/// stale-cache causes applies.
const WRITE_AFTER_REMINT_HINT: &str = "the cached token was discarded and a fresh one minted, \
     yet the write was still refused (401): not a stale cache — check the \
     principal/key mapping or a server-side revocation";

/// The cause teregen reports with a `401` (`Error.hint` in its OpenAPI spec).
///
/// Parsed from a closed vocabulary so the server's text is never echoed: an
/// unknown value is `None`, and [`explanation`](Self::explanation) is mtui's
/// own prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthHint {
    /// The challenge nonce was already used or has expired.
    ChallengeExpiredOrUsed,
    /// The signature was rejected: unknown principal, no usable key, a key
    /// mismatch or a bad signature; teregen does not say which.
    SignatureRejected,
    /// teregen could not load its key list.
    KeyListUnavailable,
    /// The bearer token is not known to this teregen.
    TokenUnknown,
    /// The bearer token has expired.
    TokenExpired,
    /// The request carried no bearer token.
    TokenMissing,
}

impl AuthHint {
    /// The hint for teregen's wire value, or `None` for anything else.
    #[must_use]
    pub fn parse(wire: &str) -> Option<Self> {
        Some(match wire {
            "challenge_expired_or_used" => Self::ChallengeExpiredOrUsed,
            "signature_rejected" => Self::SignatureRejected,
            "key_list_unavailable" => Self::KeyListUnavailable,
            "token_unknown" => Self::TokenUnknown,
            "token_expired" => Self::TokenExpired,
            "token_missing" => Self::TokenMissing,
            _ => return None,
        })
    }

    /// What the cause means for the person holding the credentials.
    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            Self::ChallengeExpiredOrUsed => {
                "the challenge nonce was already used or has expired (nonces are single-use \
                 and live 60s); retry"
            }
            Self::SignatureRejected => {
                "teregen rejected the signature: the principal is not listed, has no usable \
                 key, or the signing key does not match the configured one; it does not say \
                 which"
            }
            Self::KeyListUnavailable => {
                "teregen could not load its key list: a server-side outage that refuses \
                 every principal, not a problem with this key"
            }
            Self::TokenUnknown => {
                "the bearer token is unknown to this teregen: it was revoked, or minted by \
                 another instance"
            }
            Self::TokenExpired => {
                "the bearer token has expired; if a fresh one is refused too, check the \
                 server clock"
            }
            Self::TokenMissing => {
                "teregen saw no bearer token: a proxy may have dropped the Authorization \
                 header"
            }
        }
    }
}

/// The [`AuthHint`] in a `401` response body, or `None` when the body is
/// unreadable, not a JSON object, or carries no known `hint`.
async fn read_hint(response: reqwest::Response) -> Option<AuthHint> {
    let bytes = read_body_capped(response, MAX_API_BODY).await.ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    AuthHint::parse(value.get("hint")?.as_str()?)
}

/// The string `field` of a `200` auth response's JSON body, `None` when it is
/// absent or not a string. `what` names the endpoint in the not-JSON error.
async fn str_field(
    response: reqwest::Response,
    what: &str,
    field: &str,
) -> Result<Option<String>, TeregenAuthError> {
    let bytes = read_body_capped(response, MAX_API_BODY).await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| TeregenAuthError::Protocol(format!("{what} response is not JSON: {e}")))?;
    Ok(value.get(field).and_then(Value::as_str).map(str::to_owned))
}

/// Which request a [`TeregenAuthError::Unauthorized`] answers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthStage {
    /// `POST /auth/ssh/verify` refused the mint.
    Verify,
    /// The write was refused after a fresh mint (the retry-once path).
    WriteRetry,
}

impl std::fmt::Display for AuthStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthStage::Verify => write!(f, "/auth/ssh/verify"),
            AuthStage::WriteRetry => write!(f, "the write after a fresh mint"),
        }
    }
}

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

    /// A `401` — either from `/auth/ssh/verify` or from the write itself
    /// after a fresh mint. The principal, key and server are the non-secret
    /// locators that decide among the indistinguishable causes.
    #[error(
        "teregen auth was refused (401) for principal \"{principal}\", key {key}, at {stage} on {base}: {hint}"
    )]
    #[non_exhaustive]
    Unauthorized {
        /// The oscrc principal that was sent.
        principal: String,
        /// The `SHA256:…` fingerprint of the key that actually signed.
        key: String,
        /// Which request was refused.
        stage: AuthStage,
        /// The sanitized teregen base URL that refused: both construction sites
        /// run it through [`sanitize_url`], so a caller logging this public field
        /// cannot leak embedded credentials.
        base: String,
        /// What the refusal means: [`AuthHint::explanation`] when teregen gave
        /// a known hint, else the generic list of possible causes.
        hint: String,
        /// The cause teregen reported, when it reported a known one.
        server_hint: Option<AuthHint>,
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
    /// [`TeregenAuth::invalidate`]; a mint's own cache-write failure is logged
    /// and does not fail the mint).
    #[error("teregen token store error: {0}")]
    Store(String),
}

/// `true` when `s` is exactly 64 lowercase hex characters.
fn is_valid_nonce(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// SSH-signature bearer-token auth for teregen's `/api/v2`.
///
/// Exactly one of `sshkey_path`/`sshkey_fingerprint` (via [`Signer`])
/// identifies the signing key — the pair produced by
/// [`crate::obs::oscrc::read_credentials`]; teregen reuses the oscrc identity.
pub struct TeregenAuth<A: AgentKeys = RusshAgent> {
    base: String,
    principal: String,
    namespace: String,
    signer: Signer<A>,
    store: Option<TokenStore>,
    http: HttpClient,
    /// The fingerprint of the key that last signed a verify request, so a `401`
    /// names the key that actually signed (file-backed setups have no
    /// fingerprint configured). Sharing across concurrent calls is benign: the
    /// signer is fixed per instance, so every writer records the same key.
    last_key: std::sync::Mutex<Option<String>>,
}

impl<A: AgentKeys> std::fmt::Debug for TeregenAuth<A> {
    /// Manual impl: the struct holds no token field, but a derive would print
    /// the `HttpClient`/store internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeregenAuth")
            .field("base", &sanitize_url(&self.base))
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
            last_key: std::sync::Mutex::new(None),
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

    /// The non-secret signing-key locator (see [`Signer::key_descriptor`]):
    /// the [`signed_key`](Self::signed_key) fallback when nothing has signed
    /// yet.
    #[must_use]
    fn key_descriptor(&self) -> String {
        self.signer.key_descriptor()
    }

    /// Challenge + verify + cache a fresh token unconditionally, bypassing any
    /// cached token. The diagnostic entry point for `teregen_login`; prefer
    /// [`token`](Self::token) for normal calls. A failed mint leaves the
    /// previous cache entry alone.
    ///
    /// # Errors
    ///
    /// See [`challenge`](Self::challenge) / [`verify`](Self::verify).
    pub async fn refresh(&self) -> Result<String, TeregenAuthError> {
        self.mint().await
    }

    /// The fingerprint of the key that last signed a verify request, falling
    /// back to the configured locator when nothing has signed yet.
    fn signed_key(&self) -> String {
        self.last_key
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| self.key_descriptor())
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
        let response = self
            .post_json(&url, &json!({ "username": self.principal }))
            .await?;

        match response.status().as_u16() {
            200 => str_field(response, "challenge", "nonce")
                .await?
                .ok_or_else(|| {
                    TeregenAuthError::Protocol("challenge response is missing 'nonce'".to_owned())
                }),
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
    /// The nonce is shape-checked before signing, so a malformed challenge
    /// response never reaches the agent or the wire.
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
        // Record what actually signed (canonical `SHA256:…` form), so a `401`
        // names it even for file-backed setups.
        if let Ok(mut guard) = self.last_key.lock() {
            *guard = Some(
                sig.public_key()
                    .fingerprint(ssh_key::HashAlg::Sha256)
                    .to_string(),
            );
        }

        let url = format!("{}/auth/ssh/verify", self.base);
        tracing::debug!("teregen auth: verifying at {}", sanitize_url(&url));
        let body = json!({
            "username": self.principal,
            "nonce": nonce,
            "signature": armored,
        });
        let response = self.post_json(&url, &body).await?;

        match response.status().as_u16() {
            200 => str_field(response, "verify", "token")
                .await?
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    TeregenAuthError::Protocol(
                        "verify response is missing a non-empty 'token'".to_owned(),
                    )
                }),
            401 => Err(self
                .unauthorized(response, AuthStage::Verify, UNAUTHORIZED_HINT)
                .await),
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
    /// caches the result. A cache-write failure is logged and does not fail the
    /// mint.
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
    ///
    /// A verify refused as `challenge_expired_or_used` is retried once with a
    /// new challenge — never more, as challenges are rate-limited — and
    /// whatever the second attempt answers is surfaced.
    async fn mint(&self) -> Result<String, TeregenAuthError> {
        let token = match self.challenge_and_verify().await {
            Err(TeregenAuthError::Unauthorized {
                server_hint: Some(AuthHint::ChallengeExpiredOrUsed),
                ..
            }) => self.challenge_and_verify().await?,
            other => other?,
        };
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

    async fn challenge_and_verify(&self) -> Result<String, TeregenAuthError> {
        let nonce = self.challenge().await?;
        self.verify(&nonce).await
    }

    /// Drop any cached token, forcing the next [`token`](Self::token) call to
    /// mint (used on a `401` from an authenticated request).
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

    /// Send `method url` with `Authorization: Bearer <token>` attached, after
    /// `customize` has run on the request builder — e.g. to attach a body and
    /// extra headers for a `PUT`.
    ///
    /// On a `401`, [`invalidate`](Self::invalidate)s the cached token, mints
    /// once, and retries **once** more. A second `401` (or any other error) is
    /// surfaced without a further attempt: the challenge endpoint is
    /// rate-limited and a mint costs an ssh-agent round trip, so a retry loop
    /// against an unauthorised principal would burn the budget in a second.
    ///
    /// `customize` is `Fn`, not `FnOnce`: it is applied again on the re-mint
    /// resend, so a `PUT`'s body and headers are not dropped from the retry.
    ///
    /// Retrying a write is safe *only* because a `401` is refused by the server
    /// before any write happens (`require_auth` is the first check in teregen's
    /// `upload`), so a retried `PUT` is the *first* write to land.
    ///
    /// # Errors
    ///
    /// See [`token`](Self::token); a transport failure on either attempt
    /// surfaces as [`TeregenAuthError::Transport`].
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
            return Err(self
                .unauthorized(response, AuthStage::WriteRetry, WRITE_AFTER_REMINT_HINT)
                .await);
        }
        Ok(response)
    }

    async fn post_json(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, TeregenAuthError> {
        Ok(self
            .http
            .inner()
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(HttpError::from)?)
    }

    /// The refusal for a `401` `response`, explained by the server's known
    /// hint or else by `fallback_hint`.
    async fn unauthorized(
        &self,
        response: reqwest::Response,
        stage: AuthStage,
        fallback_hint: &'static str,
    ) -> TeregenAuthError {
        let server_hint = read_hint(response).await;
        TeregenAuthError::Unauthorized {
            principal: self.principal.clone(),
            key: self.signed_key(),
            stage,
            base: sanitize_url(&self.base),
            hint: server_hint
                .map_or(fallback_hint, AuthHint::explanation)
                .to_owned(),
            server_hint,
        }
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
        // teregen sends `token_missing` only on its JSON branch.
        let builder = self
            .http
            .inner()
            .request(method, url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
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
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, mtui_types::fixtures::SSH_ID_ED25519).unwrap();
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
        assert!(
            WRITE_AFTER_REMINT_HINT.contains("not a stale cache"),
            "write hint must say the cache was already refreshed: {WRITE_AFTER_REMINT_HINT}"
        );
        assert_ne!(UNAUTHORIZED_HINT, WRITE_AFTER_REMINT_HINT);
    }

    /// Before any signature the 401 key falls back to the configured locator
    /// (defense-in-depth: every 401 below follows a signature, so production
    /// never takes this path).
    #[test]
    fn signed_key_falls_back_to_the_configured_locator_before_any_signature() {
        let auth = TeregenAuth::new(
            "https://qam.example.invalid/api/v2".to_owned(),
            "alice".to_owned(),
            None,
            Some("SHA256:abc".to_owned()),
            HttpClient::new(crate::http::VerifyPolicy::Default(true)).unwrap(),
        );
        assert_eq!(auth.principal(), "alice");
        assert_eq!(auth.signed_key(), "SHA256:abc");
    }

    #[test]
    fn signer_key_descriptor_names_fingerprint_or_file() {
        use crate::sshsig::Signer;
        let with_fp: Signer = Signer::new(None, Some("SHA256:abc".to_owned()), "teregen auth");
        assert_eq!(with_fp.key_descriptor(), "SHA256:abc");
        let bare_fp: Signer = Signer::new(None, Some("abc".to_owned()), "teregen auth");
        assert_eq!(bare_fp.key_descriptor(), "SHA256:abc");
        let file_backed: Signer = Signer::new(
            Some(std::path::PathBuf::from("/home/user/.ssh/id_ed25519")),
            None,
            "teregen auth",
        );
        assert_eq!(file_backed.key_descriptor(), "file key");
    }

    /// `Unauthorized`'s `base` is stored already sanitized (both
    /// construction sites run it through [`sanitize_url`]), so `Display` and
    /// `Debug` print it verbatim: build the error the way the constructors do
    /// and check both renderings keep the host while dropping credentials.
    /// The end-to-end half — constructors really sanitize — is
    /// `userinfo_in_base_never_reaches_unauthorized_error` in
    /// `tests/teregen_auth.rs`.
    #[test]
    fn unauthorized_base_is_stored_sanitized() {
        // Exhaustive on purpose: a new field must revisit this discipline.
        let err = TeregenAuthError::Unauthorized {
            principal: "alice".to_owned(),
            key: "SHA256:abc".to_owned(),
            stage: AuthStage::Verify,
            base: sanitize_url("https://user:pass@qam.example.invalid/api/v2"),
            hint: "hint".to_owned(),
            server_hint: None,
        };
        for rendered in [format!("{err}"), format!("{err:?}")] {
            assert!(!rendered.contains("user:pass"), "leaked: {rendered}");
            assert!(
                rendered.contains("qam.example.invalid"),
                "host missing: {rendered}"
            );
        }
    }
}
