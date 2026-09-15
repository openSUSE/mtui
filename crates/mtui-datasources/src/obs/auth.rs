//! OBS "Signature" (SSH) authentication for the native OBS backend.
//!
//! Implements the OBS Signature challenge/response in-process (no `osc`, no
//! subprocess): the first request goes out unauthenticated (the session may
//! already hold a cookie); on a `401` the `WWW-Authenticate: Signature` realm is
//! read, an SSHSIG over `(created): <epoch>` is built (see
//! [`crate::obs::sshsig`]), and the request is resent exactly once with the
//! `Authorization: Signature` header — which is **never logged**, since this
//! module only returns it to the transport ([`crate::obs::client`]).
//!
//! Any OpenSSH key type works — Ed25519, ECDSA, and RSA private-key files, plus
//! keys held by a running ssh-agent (selected by `SHA256:…` fingerprint or as
//! the passphrase-protected counterpart of a file on disk). Under headless
//! `mtui-mcp` we must never block on a passphrase prompt, so an encrypted key is
//! only usable via the agent; every unresolvable case fails closed with a typed
//! [`ObsError::Config`].

use std::path::PathBuf;

use ssh_key::{HashAlg, PublicKey, Signature};

use crate::obs::client::ObsAuth;
use crate::obs::errors::ObsError;
use crate::obs::sshsig;
use crate::sshsig::Signer;

/// The [`Signer`] context label for the native OBS backend's fail-closed
/// hints ("… {context} never prompts for a passphrase").
const CONTEXT: &str = "the native OBS backend";

/// The current wall-clock Unix timestamp, seamed for deterministic tests.
///
/// A signed `i64` matches `chrono`'s epoch type used elsewhere in the crate.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A minimal ssh-agent surface: list public identities and sign raw bytes.
///
/// Abstracted so the agent-selection logic is unit-testable offline against a
/// mock; [`RusshAgent`] drives russh's agent client over `$SSH_AUTH_SOCK`.
#[async_trait::async_trait]
pub trait AgentKeys: Send {
    /// The public keys the agent currently holds (fails closed on error).
    async fn identities(&mut self) -> Result<Vec<PublicKey>, ObsError>;

    /// Ask the agent to sign `data` with the key identified by `public`.
    ///
    /// `hash_alg` selects the RSA signature algorithm (`Some(Sha512)` →
    /// `rsa-sha2-512`); it is `None` for Ed25519/ECDSA which have one algorithm.
    async fn sign(
        &mut self,
        public: &PublicKey,
        hash_alg: Option<HashAlg>,
        data: &[u8],
    ) -> Result<Signature, ObsError>;
}

/// The production [`AgentKeys`] backed by russh's ssh-agent client.
///
/// Connects lazily on first use via `$SSH_AUTH_SOCK` (`connect_env`), so
/// building [`ObsSignatureAuth`] never touches the agent — only an actual
/// challenge that needs the agent does.
#[derive(Default)]
pub struct RusshAgent {
    client: Option<russh::keys::agent::client::AgentClient<tokio::net::UnixStream>>,
}

impl RusshAgent {
    async fn client(
        &mut self,
    ) -> Result<&mut russh::keys::agent::client::AgentClient<tokio::net::UnixStream>, ObsError>
    {
        if self.client.is_none() {
            let c = russh::keys::agent::client::AgentClient::connect_env()
                .await
                .map_err(|e| ObsError::Config(format!("could not query the ssh-agent: {e}")))?;
            self.client = Some(c);
        }
        // Just set above when None.
        Ok(self.client.as_mut().expect("agent client set"))
    }
}

#[async_trait::async_trait]
impl AgentKeys for RusshAgent {
    async fn identities(&mut self) -> Result<Vec<PublicKey>, ObsError> {
        let client = self.client().await?;
        let ids = client
            .request_identities()
            .await
            .map_err(|e| ObsError::Config(format!("could not query the ssh-agent: {e}")))?;
        Ok(ids
            .into_iter()
            .map(|id| id.public_key().into_owned())
            .collect())
    }

    async fn sign(
        &mut self,
        public: &PublicKey,
        hash_alg: Option<HashAlg>,
        data: &[u8],
    ) -> Result<Signature, ObsError> {
        let client = self.client().await?;
        client
            .sign_request_signature(public, hash_alg, data)
            .await
            .map_err(|e| ObsError::Config(format!("ssh-agent signing failed: {e}")))
    }
}

/// A `requests`-style auth handler for OBS SSH-signature authentication.
///
/// Exactly one of `sshkey_path` (a private-key file) or `sshkey_fingerprint`
/// (an ssh-agent key's `SHA256:…` fingerprint) identifies the signing key —
/// exactly the pair produced by [`crate::obs::oscrc`]. Key resolution itself is
/// [`Signer`], shared with teregen auth.
pub struct ObsSignatureAuth<A: AgentKeys = RusshAgent> {
    user: String,
    signer: Signer<A>,
}

impl ObsSignatureAuth<RusshAgent> {
    /// Build with the acting `user` and its oscrc key locator, using the real
    /// ssh-agent (`$SSH_AUTH_SOCK`) for agent-backed keys.
    #[must_use]
    pub fn new(
        user: String,
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
    ) -> Self {
        Self::with_agent(user, sshkey_path, sshkey_fingerprint, RusshAgent::default())
    }
}

impl<A: AgentKeys> ObsSignatureAuth<A> {
    /// Build with an explicit [`AgentKeys`] backend (used by tests).
    pub fn with_agent(
        user: String,
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
        agent: A,
    ) -> Self {
        Self {
            user,
            signer: Signer::with_agent(sshkey_path, sshkey_fingerprint, CONTEXT, agent),
        }
    }

    /// Build the `Authorization: Signature …` header value for `realm`.
    ///
    /// The signature is only ever returned to the caller, never logged.
    ///
    /// # Errors
    ///
    /// Returns [`ObsError::Config`] for any unresolvable key/agent case
    /// (fail-closed; never prompts for a passphrase).
    pub async fn authorization(&self, realm: &str) -> Result<String, ObsError> {
        let created = now_unix();
        let blob = self.sign(realm, created).await?;
        Ok(format!(
            "Signature keyId=\"{}\",algorithm=\"ssh\",headers=\"(created)\",created={created},signature=\"{blob}\"",
            self.user
        ))
    }

    /// Resolve the configured locator to a key and produce the base64 SSHSIG.
    async fn sign(&self, realm: &str, created: i64) -> Result<String, ObsError> {
        let msg = sshsig::created_message(created);
        let sig = self.signer.sign(realm, &msg).await?;
        Ok(crate::sshsig::encode_bare(&sig)?)
    }
}

#[async_trait::async_trait]
impl<A: AgentKeys + Sync> ObsAuth for ObsSignatureAuth<A> {
    async fn authorization(&self, realm: &str) -> Result<Option<String>, ObsError> {
        self.authorization(realm).await.map(Some)
    }
}

/// Parse `WWW-Authenticate` into `{scheme: {param: value}}`.
///
/// Reads **every** `WWW-Authenticate` header value separately (reqwest's
/// `HeaderMap::get_all` preserves duplicates rather than comma-merging them
/// into one unparseable string), so a second challenge (e.g.
/// `Basic` alongside `Signature`) never hides `Signature`. Blank lines are
/// skipped; a param-less scheme yields an empty map; a malformed param list
/// yields an empty map. Scheme names are lowercased.
#[must_use]
pub(crate) fn challenge_params(
    headers: &reqwest::header::HeaderMap,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    use reqwest::header::WWW_AUTHENTICATE;

    let mut schemes = std::collections::BTreeMap::new();
    for value in headers.get_all(WWW_AUTHENTICATE) {
        let Ok(line) = value.to_str() else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (scheme, rest) = line.split_once(' ').unwrap_or((line, ""));
        let params = if rest.trim().is_empty() {
            std::collections::BTreeMap::new()
        } else {
            parse_auth_params(rest)
        };
        schemes.insert(scheme.to_ascii_lowercase(), params);
    }
    schemes
}

/// Parse a comma-separated `key="value"` / `key=value` auth-param list.
///
/// A token without an `=` makes the whole list malformed → an empty map.
/// Surrounding double-quotes are stripped from values.
fn parse_auth_params(rest: &str) -> std::collections::BTreeMap<String, String> {
    let mut params = std::collections::BTreeMap::new();
    for token in rest.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Some((k, v)) = token.split_once('=') else {
            // A bare token with no `=` is malformed for keqv → empty map.
            return std::collections::BTreeMap::new();
        };
        let v = v.trim().trim_matches('"');
        params.insert(k.trim().to_ascii_lowercase(), v.to_owned());
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_params_parses_dual_scheme() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            "Signature realm=\"Use your developer account\""
                .parse()
                .unwrap(),
        );
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            "Basic realm=\"Open Build Service\"".parse().unwrap(),
        );
        let schemes = challenge_params(&headers);
        assert_eq!(
            schemes.keys().cloned().collect::<Vec<_>>(),
            vec!["basic".to_owned(), "signature".to_owned()]
        );
        assert_eq!(schemes["signature"]["realm"], "Use your developer account");
        assert_eq!(schemes["basic"]["realm"], "Open Build Service");
    }

    #[test]
    fn challenge_params_handles_empty_paramless_and_malformed() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(reqwest::header::WWW_AUTHENTICATE, "".parse().unwrap());
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            "Negotiate".parse().unwrap(),
        );
        headers.append(
            reqwest::header::WWW_AUTHENTICATE,
            "Signature realm".parse().unwrap(),
        );
        let schemes = challenge_params(&headers);
        assert!(schemes.contains_key("negotiate"));
        assert!(schemes["negotiate"].is_empty());
        assert!(schemes["signature"].is_empty());
        assert_eq!(schemes.len(), 2);
    }
}
