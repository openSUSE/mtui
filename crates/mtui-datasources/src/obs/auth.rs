//! OBS "Signature" (SSH) authentication for the native OBS backend.
//!
//! Ported from upstream `mtui/data_sources/obs/auth.py`. Reproduces the OBS
//! Signature challenge/response in-process (no `osc`, no subprocess): the first
//! request goes out unauthenticated (the session may already hold a cookie); on
//! a `401` the `WWW-Authenticate: Signature` realm is read, an SSHSIG over
//! `(created): <epoch>` is built (see [`crate::obs::sshsig`]), and the request
//! is resent exactly once with the `Authorization: Signature` header. The
//! Authorization header and its signature are **never logged** — this module
//! only ever returns the header value to the transport ([`crate::obs::client`]),
//! which logs method + URL only.
//!
//! Any OpenSSH key type works — Ed25519, ECDSA, and RSA private-key files, plus
//! keys held by a running ssh-agent (selected by `SHA256:…` fingerprint or as
//! the passphrase-protected counterpart of a file on disk). Under headless
//! `mtui-mcp` we must never block on a passphrase prompt, so an encrypted key is
//! only usable via the agent; every unresolvable case fails closed with a typed
//! [`ObsError::Config`].

use std::path::{Path, PathBuf};

use ssh_key::public::KeyData;
use ssh_key::{HashAlg, PrivateKey, PublicKey, Signature};

use crate::obs::client::ObsAuth;
use crate::obs::errors::ObsError;
use crate::obs::sshsig;

/// The current wall-clock Unix timestamp, seamed for deterministic tests.
///
/// Upstream calls `int(time.time())`; a signed `i64` matches `chrono`'s epoch
/// type used elsewhere in the crate and is wide enough for any real clock.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A minimal ssh-agent surface: list public identities and sign raw bytes.
///
/// Abstracted so the agent-selection logic is unit-testable offline (a mock
/// holding known keys) without a live ssh-agent; the production implementation
/// ([`RusshAgent`]) drives russh's agent client over `$SSH_AUTH_SOCK`.
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

/// Read the public-key blob from `<path>.pub`, or `None` if absent/malformed.
///
/// Used to identify a passphrase-protected private key's counterpart among the
/// ssh-agent's loaded keys by comparing public-key data. Mirrors upstream
/// `_pubkey_blob`: any read/parse failure (absent file, too few fields,
/// non-base64) yields `None` rather than an error.
fn pubkey_data(path: &Path) -> Option<KeyData> {
    let pub_path = format!("{}.pub", path.display());
    let text = std::fs::read_to_string(pub_path).ok()?;
    PublicKey::from_openssh(text.trim())
        .ok()
        .map(|k| k.key_data().clone())
}

/// The RSA signature-algorithm selector for the agent path.
///
/// Only RSA needs an explicit `rsa-sha2-512`; Ed25519/ECDSA have a single
/// algorithm (upstream `_sig_algorithm`). The file-key path handles this inside
/// `ssh-key`'s RSA signer, which defaults to SHA-512.
fn agent_hash_alg(key: &KeyData) -> Option<HashAlg> {
    if key.is_rsa() {
        Some(HashAlg::Sha512)
    } else {
        None
    }
}

/// A `requests`-style auth handler for OBS SSH-signature authentication.
///
/// Exactly one of `sshkey_path` (a private-key file) or `sshkey_fingerprint`
/// (an ssh-agent key's `SHA256:…` fingerprint) identifies the signing key —
/// exactly the pair produced by [`crate::obs::oscrc`].
pub struct ObsSignatureAuth<A: AgentKeys = RusshAgent> {
    user: String,
    sshkey_path: Option<PathBuf>,
    sshkey_fingerprint: Option<String>,
    agent: tokio::sync::Mutex<A>,
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
            sshkey_path,
            sshkey_fingerprint,
            agent: tokio::sync::Mutex::new(agent),
        }
    }

    /// Build the `Authorization: Signature …` header value for `realm`.
    ///
    /// The signature is built here and only ever returned to the caller — it is
    /// never logged.
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
        if let Some(fingerprint) = &self.sshkey_fingerprint {
            return self
                .sign_with_agent_fingerprint(fingerprint, realm, created)
                .await;
        }
        let Some(path) = &self.sshkey_path else {
            return Err(ObsError::Config(
                "no ssh key configured for OBS signature auth".to_owned(),
            ));
        };
        self.sign_with_file(path, realm, created).await
    }

    /// Sign with a private-key file, falling back to the ssh-agent when the file
    /// is encrypted or absent (upstream `_load_key_file`).
    async fn sign_with_file(
        &self,
        path: &Path,
        realm: &str,
        created: i64,
    ) -> Result<String, ObsError> {
        match PrivateKey::read_openssh_file(path) {
            Ok(key) if !key.is_encrypted() => sshsig::sign_created(&key, realm, created),
            // Encrypted key: never prompt — use the agent counterpart.
            Ok(_) => self.sign_with_agent_for_file(path, realm, created).await,
            Err(ssh_key::Error::Io(std::io::ErrorKind::NotFound)) => {
                // Missing file may still be an agent key identified by its .pub.
                self.sign_with_agent_for_file(path, realm, created).await
            }
            Err(e) => Err(ObsError::Config(format!(
                "ssh key {} is not a usable private key ({e})",
                path.display()
            ))),
        }
    }

    /// Select an ssh-agent key by `SHA256:…` fingerprint and sign (upstream
    /// `_agent_key_by_fingerprint`). The `SHA256:` prefix is optional.
    async fn sign_with_agent_fingerprint(
        &self,
        fingerprint: &str,
        realm: &str,
        created: i64,
    ) -> Result<String, ObsError> {
        let want = fingerprint.trim();
        let mut agent = self.agent.lock().await;
        let ids = agent.identities().await?;
        for key in &ids {
            let fp = key.fingerprint(HashAlg::Sha256).to_string();
            let bare = fp.split_once(':').map_or(fp.as_str(), |(_, b)| b);
            if fp == want || bare == want {
                return self.agent_sign(&mut *agent, key, realm, created).await;
            }
        }
        Err(ObsError::Config(format!(
            "ssh-agent has no key matching fingerprint {want:?}; load it with \
             'ssh-add' (the native OBS backend never prompts for a passphrase)"
        )))
    }

    /// Find the ssh-agent key that is `path`'s decrypted counterpart, matched by
    /// public-key data from `<path>.pub` (upstream `_agent_key_for_file`).
    async fn sign_with_agent_for_file(
        &self,
        path: &Path,
        realm: &str,
        created: i64,
    ) -> Result<String, ObsError> {
        let blob = pubkey_data(path);
        if let Some(blob) = &blob {
            let mut agent = self.agent.lock().await;
            let ids = agent.identities().await?;
            for key in &ids {
                if key.key_data() == blob {
                    return self.agent_sign(&mut *agent, key, realm, created).await;
                }
            }
        }
        let hint = if blob.is_some() {
            String::new()
        } else {
            format!(" (no {}.pub found to identify it)", path.display())
        };
        Err(ObsError::Config(format!(
            "ssh key {} is passphrase-protected and no matching key is loaded in \
             the ssh-agent{hint}; run 'ssh-add {}' first — the native OBS backend \
             never prompts for a passphrase",
            path.display(),
            path.display()
        )))
    }

    /// Sign the SSHSIG-enveloped bytes via the agent and pack the outer blob.
    async fn agent_sign(
        &self,
        agent: &mut A,
        key: &PublicKey,
        realm: &str,
        created: i64,
    ) -> Result<String, ObsError> {
        let data = sshsig::agent_signed_data(realm, created)?;
        let hash_alg = agent_hash_alg(key.key_data());
        let signature = agent.sign(key, hash_alg, &data).await?;
        sshsig::pack_agent_signature(key.key_data(), realm, signature)
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
/// `HeaderMap::get_all` preserves duplicates, unlike Python `requests` which
/// comma-merges them into one unparseable string), so a second challenge (e.g.
/// `Basic` alongside `Signature`) never hides `Signature`. Blank lines are
/// skipped; a param-less scheme yields an empty map; a malformed param list
/// yields an empty map. Scheme names are lowercased. Upstream
/// `_challenge_params`.
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
/// Mirrors urllib's `parse_keqv_list(parse_http_list(...))`: a token without an
/// `=` makes the whole list malformed → an empty map (upstream catches the
/// `ValueError`). Surrounding double-quotes are stripped from values.
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

    /// A fixed OpenSSH Ed25519 public key (from the deterministic test seed),
    /// used to exercise the algorithm selector without an RNG.
    const ED25519_PUB: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAOhB7/zzhC+HXDdGOdLwJln5NYwm6UNXx3chmQSVTG4 test";

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
        // The blank line is skipped, leaving exactly two schemes.
        assert_eq!(schemes.len(), 2);
    }

    #[test]
    fn agent_hash_alg_none_for_ed25519() {
        let ed = PublicKey::from_openssh(ED25519_PUB)
            .unwrap()
            .key_data()
            .clone();
        assert!(!ed.is_rsa());
        assert_eq!(agent_hash_alg(&ed), None);
    }

    #[test]
    fn pubkey_data_none_for_absent_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("k");
        // Absent .pub.
        assert!(pubkey_data(&base).is_none());
        // Malformed .pub (not a valid key line).
        std::fs::write(dir.path().join("k.pub"), "only-one-field\n").unwrap();
        assert!(pubkey_data(&base).is_none());
        // Non-base64 body (upstream `test_pubkey_blob_missing_and_malformed`).
        std::fs::write(dir.path().join("k.pub"), "ssh-rsa @@@not-base64@@@ me\n").unwrap();
        assert!(pubkey_data(&base).is_none());
        // Valid .pub parses to key data.
        std::fs::write(dir.path().join("k.pub"), format!("{ED25519_PUB}\n")).unwrap();
        assert!(pubkey_data(&base).is_some());
    }
}
