//! OpenSSH SSHSIG wire-format signing, shared by every mtui caller that proves
//! identity with an SSH key: the native OBS backend ([`crate::obs::auth`],
//! `Authorization: Signature`, raw base64) and teregen auth
//! ([`crate::teregen::auth`], `POST .../auth/ssh/verify`, armored PEM).
//!
//! Reproduces `ssh-keygen -Y sign` for any key type (Ed25519, ECDSA, RSA), so
//! mtui signs in-process — no `ssh-keygen`/`osc` subprocess. Per OpenSSH's
//! SSHSIG ([PROTOCOL.sshsig]) a message is hashed with `sha512`, wrapped with a
//! namespace, signed, and packed with the public key into the outer blob. Two
//! encodings of that blob exist: [`encode_bare`] (raw base64, OBS's
//! `Authorization: Signature` header) and [`encode_pem`] (armored, teregen's
//! wire format).
//!
//! RSA keys are signed with `rsa-sha2-512` (`ssh-key`'s default RSA `Signer`
//! impl), not the legacy `ssh-rsa` (SHA-1) that modern OpenSSH/servers reject.
//! The blob's message-hash algorithm (`sha512`) is independent of the
//! signature algorithm and stays fixed, matching `ssh-keygen -Y sign`.
//!
//! [`Signer`] is the key-resolution engine both callers share: fingerprint →
//! agent; unencrypted file → sign in-process; encrypted or absent file → agent
//! key matched by `<path>.pub` blob; everything else fails closed, never
//! prompts for a passphrase. It is parameterised by a `context: &'static str`
//! label (e.g. "the native OBS backend" / "teregen auth") so a fail-closed
//! hint names the right caller.
//!
//! [PROTOCOL.sshsig]: https://cvsweb.openbsd.org/src/usr.bin/ssh/PROTOCOL.sshsig

use std::path::{Path, PathBuf};

use base64ct::{Base64, Encoding as _};
use ssh_key::encoding::Encode;
use ssh_key::public::KeyData;
use ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey, Signature, SshSig};
use thiserror::Error;

use crate::obs::auth::AgentKeys;

/// The fixed SSHSIG message-hash algorithm, matching `ssh-keygen -Y sign`.
const HASH_ALG: HashAlg = HashAlg::Sha512;

/// Errors from building or resolving an SSHSIG signature.
///
/// Fail-closed and secret-safe: no variant ever carries key material or a
/// signature, only paths, fingerprints and error text from `ssh-key`/the
/// agent.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SshSigError {
    /// Neither a key file nor an agent fingerprint is configured.
    #[error("no ssh key configured for {context}")]
    NoKeyConfigured {
        /// The caller naming this signer (e.g. "the native OBS backend").
        context: &'static str,
    },

    /// A configured private-key file could not be read as a usable key at all
    /// (distinct from "encrypted" — that falls back to the agent instead).
    #[error("ssh key {} is not a usable private key ({source})", path.display())]
    UnusableKeyFile {
        /// The configured private-key file path.
        path: PathBuf,
        /// The underlying `ssh-key` parse failure.
        #[source]
        source: ssh_key::Error,
    },

    /// No loaded ssh-agent identity matches the configured fingerprint.
    #[error(
        "ssh-agent has no key matching fingerprint {fingerprint:?}; load it with \
         'ssh-add' ({context} never prompts for a passphrase)"
    )]
    NoAgentKey {
        /// The configured `SHA256:…` fingerprint (or its bare form).
        fingerprint: String,
        /// The caller naming this signer.
        context: &'static str,
    },

    /// A private-key file is encrypted (or absent) and no matching key is
    /// loaded in the ssh-agent.
    #[error("{}", passphrase_protected_message(path, *hint_available, context))]
    PassphraseProtected {
        /// The configured private-key file path.
        path: PathBuf,
        /// Whether a `<path>.pub` blob was found to identify the agent key by.
        hint_available: bool,
        /// The caller naming this signer.
        context: &'static str,
    },

    /// The ssh-agent itself failed (connect, list identities, or sign) —
    /// verbatim text from the [`AgentKeys`] backend, which already states the
    /// failing operation.
    #[error("{0}")]
    Agent(String),

    /// `ssh-key` failed to build, assemble or encode the SSHSIG envelope.
    #[error("could not {operation} the SSHSIG {part}: {source}")]
    Sshsig {
        /// What was being done: "sign", "build", "assemble", "encode".
        operation: &'static str,
        /// What was being acted on: "payload", "signature".
        part: &'static str,
        /// The underlying `ssh-key` failure.
        #[source]
        source: ssh_key::Error,
    },
}

/// Render [`SshSigError::PassphraseProtected`]'s message, mirroring the
/// original OBS-only wording with the caller's `context` substituted in.
fn passphrase_protected_message(path: &Path, hint_available: bool, context: &str) -> String {
    let hint = if hint_available {
        String::new()
    } else {
        format!(" (no {}.pub found to identify it)", path.display())
    };
    format!(
        "ssh key {} is passphrase-protected and no matching key is loaded in \
         the ssh-agent{hint}; run 'ssh-add {}' first — {context} never prompts \
         for a passphrase",
        path.display(),
        path.display()
    )
}

/// Build the SSHSIG over `msg` using a file-backed private key of any
/// supported type.
///
/// `namespace` is the SSHSIG namespace. Returns the assembled [`SshSig`];
/// encode it with [`encode_bare`] or [`encode_pem`] depending on the wire
/// format the caller needs.
///
/// # Errors
///
/// Returns [`SshSigError::Sshsig`] if the key's algorithm is unsupported or
/// the crypto backend fails; fail-closed, and the message never leaks key
/// material.
pub fn sign_message(key: &PrivateKey, namespace: &str, msg: &[u8]) -> Result<SshSig, SshSigError> {
    SshSig::sign(key, namespace, HASH_ALG, msg).map_err(|source| SshSigError::Sshsig {
        operation: "sign",
        part: "payload",
        source,
    })
}

/// The pre-hashed enveloped bytes an ssh-agent must sign for the agent path.
///
/// The agent signs raw bytes, so the caller asks it to sign exactly this
/// (`MAGIC | namespace | reserved | hash_alg | H(message)`), then hands the
/// resulting [`Signature`] plus the agent key's public data to [`pack`].
///
/// # Errors
///
/// Returns [`SshSigError::Sshsig`] if the namespace is empty (SSHSIG forbids
/// it).
pub fn signed_data(namespace: &str, msg: &[u8]) -> Result<Vec<u8>, SshSigError> {
    SshSig::signed_data(namespace, HASH_ALG, msg).map_err(|source| SshSigError::Sshsig {
        operation: "build",
        part: "payload",
        source,
    })
}

/// Pack an ssh-agent's raw [`Signature`] over [`signed_data`] into an
/// [`SshSig`].
///
/// # Errors
///
/// Returns [`SshSigError::Sshsig`] if the public key / namespace / signature
/// cannot be assembled into a valid SSHSIG (fail-closed).
pub fn pack(
    public: &KeyData,
    namespace: &str,
    signature: Signature,
) -> Result<SshSig, SshSigError> {
    SshSig::new(public.clone(), namespace, HASH_ALG, signature).map_err(|source| {
        SshSigError::Sshsig {
            operation: "assemble",
            part: "signature",
            source,
        }
    })
}

/// Encode `sig` as raw base64 with no PEM armor — the OBS `Authorization:
/// Signature` wire format.
///
/// # Errors
///
/// Returns [`SshSigError::Sshsig`] if `ssh-key` fails to encode the blob.
pub fn encode_bare(sig: &SshSig) -> Result<String, SshSigError> {
    let mut bytes = Vec::new();
    sig.encode(&mut bytes)
        .map_err(|source| SshSigError::Sshsig {
            operation: "encode",
            part: "signature",
            source: ssh_key::Error::Encoding(source),
        })?;
    Ok(Base64::encode_string(&bytes))
}

/// Encode `sig` as an armored PEM (`-----BEGIN SSH SIGNATURE-----`) — teregen's
/// wire format.
///
/// # Errors
///
/// Returns [`SshSigError::Sshsig`] if `ssh-key` fails to encode the blob.
pub fn encode_pem(sig: &SshSig) -> Result<String, SshSigError> {
    sig.to_pem(LineEnding::LF)
        .map_err(|source| SshSigError::Sshsig {
            operation: "encode",
            part: "signature",
            source,
        })
}

/// The non-secret signing-key locator for diagnostics: the `SHA256:…`
/// fingerprint when one is configured, else `file key` (never a path, so
/// home directories stay off the MCP surface). A bare fingerprint without
/// the prefix (which signing accepts) is normalised to the canonical form
/// that `verify()` records from the signature. The single wording behind
/// [`Signer::key_descriptor`], the teregen `401` error and the
/// `whoami`/`teregen_login` displays.
#[must_use]
pub fn key_descriptor(sshkey_fingerprint: Option<&str>) -> String {
    match sshkey_fingerprint {
        Some(fingerprint) if fingerprint.contains(':') => fingerprint.to_owned(),
        Some(bare) => format!("SHA256:{bare}"),
        None => "file key".to_owned(),
    }
}

/// `<path>.pub` as a path operation, not a string one: `format!("{}.pub",
/// path.display())` would lossily replace non-UTF8 sequences and miss the
/// sibling. Same shape as `key_available` in [`crate::obs::oscrc`].
fn pub_path(path: &Path) -> PathBuf {
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    PathBuf::from(pub_path)
}

/// The SHA256 fingerprint of the file-backed key at `path`, if derivable while
/// keeping secrets out of the output: from the public half of an unencrypted
/// private key first (mirroring [`Signer`]'s resolution precedence, which signs
/// such a key in-process — a stale `<path>.pub` must never misreport it), else
/// from `<path>.pub` when the private file is encrypted or absent (the two
/// cases `sign_with_file` defers to the agent for — anything else signs
/// fail-closed, so the display must not name a fingerprint either). Only the
/// public half is fingerprinted; private key bytes never reach the output.
/// Anything else yields `None` rather than prompting — the caller falls back
/// to `file key`.
#[must_use]
pub fn file_key_fingerprint(path: &Path) -> Option<String> {
    match PrivateKey::read_openssh_file(path) {
        Ok(key) if !key.is_encrypted() => {
            return Some(key.public_key().fingerprint(HashAlg::Sha256).to_string());
        }
        Ok(_) | Err(ssh_key::Error::Io(std::io::ErrorKind::NotFound)) => {}
        Err(_) => return None,
    }
    let text = std::fs::read_to_string(pub_path(path)).ok()?;
    PublicKey::from_openssh(text.trim())
        .ok()
        .map(|pubkey| pubkey.fingerprint(HashAlg::Sha256).to_string())
}

/// The non-secret signing-key display for `whoami`/`teregen_login`: the
/// configured fingerprint when one is set, else the file-backed key's SHA256
/// fingerprint when derivable, else `file key` (never a path, so home
/// directories stay off the MCP surface).
#[must_use]
pub fn key_display(sshkey_path: Option<&Path>, sshkey_fingerprint: Option<&str>) -> String {
    if sshkey_fingerprint.is_some() {
        return key_descriptor(sshkey_fingerprint);
    }
    if let Some(path) = sshkey_path
        && let Some(fingerprint) = file_key_fingerprint(path)
    {
        return fingerprint;
    }
    "file key".to_owned()
}

/// The shared key-resolution engine: fingerprint → agent; unencrypted file →
/// sign in-process; encrypted or absent file → agent key matched by
/// `<path>.pub` blob; everything else fails closed, never prompts.
///
/// Exactly one of `sshkey_path` / `sshkey_fingerprint` identifies the signing
/// key — the pair produced by [`crate::obs::oscrc`]. `context` names the
/// caller in fail-closed hints (e.g. "the native OBS backend", "teregen
/// auth").
pub struct Signer<A: AgentKeys = crate::obs::auth::RusshAgent> {
    sshkey_path: Option<PathBuf>,
    sshkey_fingerprint: Option<String>,
    context: &'static str,
    agent: tokio::sync::Mutex<A>,
}

impl Signer<crate::obs::auth::RusshAgent> {
    /// Build with an oscrc key locator, using the real ssh-agent
    /// (`$SSH_AUTH_SOCK`) for agent-backed keys.
    #[must_use]
    pub fn new(
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
        context: &'static str,
    ) -> Self {
        Self::with_agent(
            sshkey_path,
            sshkey_fingerprint,
            context,
            crate::obs::auth::RusshAgent::default(),
        )
    }
}

impl<A: AgentKeys> Signer<A> {
    /// Build with an explicit [`AgentKeys`] backend (used by tests).
    pub fn with_agent(
        sshkey_path: Option<PathBuf>,
        sshkey_fingerprint: Option<String>,
        context: &'static str,
        agent: A,
    ) -> Self {
        Self {
            sshkey_path,
            sshkey_fingerprint,
            context,
            agent: tokio::sync::Mutex::new(agent),
        }
    }

    /// The non-secret key locator for diagnostics: the `SHA256:…`
    /// fingerprint when configured, else `file key` (never a path, so home
    /// directories stay off the MCP surface). A bare fingerprint without the
    /// prefix (which signing accepts) is normalised to the canonical form
    /// that `verify()` records from the signature.
    #[must_use]
    pub fn key_descriptor(&self) -> String {
        key_descriptor(self.sshkey_fingerprint.as_deref())
    }

    /// Resolve the configured locator to a key and sign `msg` under
    /// `namespace`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SshSigError`] for any unresolvable key/agent case;
    /// never prompts for a passphrase.
    pub async fn sign(&self, namespace: &str, msg: &[u8]) -> Result<SshSig, SshSigError> {
        if let Some(fingerprint) = &self.sshkey_fingerprint {
            return self
                .sign_with_agent_fingerprint(fingerprint, namespace, msg)
                .await;
        }
        let Some(path) = &self.sshkey_path else {
            return Err(SshSigError::NoKeyConfigured {
                context: self.context,
            });
        };
        self.sign_with_file(path, namespace, msg).await
    }

    /// Sign with a private-key file, falling back to the ssh-agent when the
    /// file is encrypted or absent.
    async fn sign_with_file(
        &self,
        path: &Path,
        namespace: &str,
        msg: &[u8],
    ) -> Result<SshSig, SshSigError> {
        match PrivateKey::read_openssh_file(path) {
            Ok(key) if !key.is_encrypted() => sign_message(&key, namespace, msg),
            // Encrypted key: never prompt — use the agent counterpart.
            Ok(_) => self.sign_with_agent_for_file(path, namespace, msg).await,
            Err(ssh_key::Error::Io(std::io::ErrorKind::NotFound)) => {
                // Missing file may still be an agent key identified by its .pub.
                self.sign_with_agent_for_file(path, namespace, msg).await
            }
            Err(source) => Err(SshSigError::UnusableKeyFile {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Select an ssh-agent key by `SHA256:…` fingerprint and sign. The
    /// `SHA256:` prefix is optional.
    async fn sign_with_agent_fingerprint(
        &self,
        fingerprint: &str,
        namespace: &str,
        msg: &[u8],
    ) -> Result<SshSig, SshSigError> {
        let want = fingerprint.trim();
        let mut agent = self.agent.lock().await;
        let ids = agent
            .identities()
            .await
            .map_err(|e| SshSigError::Agent(e.to_string()))?;
        for key in &ids {
            let fp = key.fingerprint(HashAlg::Sha256).to_string();
            let bare = fp.split_once(':').map_or(fp.as_str(), |(_, b)| b);
            if fp == want || bare == want {
                return self.agent_sign(&mut agent, key, namespace, msg).await;
            }
        }
        Err(SshSigError::NoAgentKey {
            fingerprint: want.to_owned(),
            context: self.context,
        })
    }

    /// Find the ssh-agent key that is `path`'s decrypted counterpart, matched
    /// by public-key data from `<path>.pub`.
    async fn sign_with_agent_for_file(
        &self,
        path: &Path,
        namespace: &str,
        msg: &[u8],
    ) -> Result<SshSig, SshSigError> {
        let blob = pubkey_data(path);
        if let Some(blob) = &blob {
            let mut agent = self.agent.lock().await;
            let ids = agent
                .identities()
                .await
                .map_err(|e| SshSigError::Agent(e.to_string()))?;
            for key in &ids {
                if key.key_data() == blob {
                    return self.agent_sign(&mut agent, key, namespace, msg).await;
                }
            }
        }
        Err(SshSigError::PassphraseProtected {
            path: path.to_path_buf(),
            hint_available: blob.is_some(),
            context: self.context,
        })
    }

    /// Sign the SSHSIG-enveloped bytes via the agent and pack the outer blob.
    async fn agent_sign(
        &self,
        agent: &mut A,
        key: &PublicKey,
        namespace: &str,
        msg: &[u8],
    ) -> Result<SshSig, SshSigError> {
        let data = signed_data(namespace, msg)?;
        let hash_alg = agent_hash_alg(key.key_data());
        let signature = agent
            .sign(key, hash_alg, &data)
            .await
            .map_err(|e| SshSigError::Agent(e.to_string()))?;
        pack(key.key_data(), namespace, signature)
    }
}

/// Read the public-key blob from `<path>.pub`, or `None` if absent/malformed.
///
/// Identifies a passphrase-protected private key's counterpart among the
/// agent's loaded keys by public-key data. Any read/parse failure yields
/// `None` rather than an error.
fn pubkey_data(path: &Path) -> Option<KeyData> {
    let text = std::fs::read_to_string(pub_path(path)).ok()?;
    PublicKey::from_openssh(text.trim())
        .ok()
        .map(|k| k.key_data().clone())
}

/// The RSA signature-algorithm selector for the agent path.
///
/// Only RSA needs an explicit `rsa-sha2-512`; Ed25519/ECDSA have one
/// algorithm.
fn agent_hash_alg(key: &KeyData) -> Option<HashAlg> {
    if key.is_rsa() {
        Some(HashAlg::Sha512)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed OpenSSH Ed25519 public key, used to exercise the algorithm
    /// selector without an RNG.
    const ED25519_PUB: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAOhB7/zzhC+HXDdGOdLwJln5NYwm6UNXx3chmQSVTG4 test";

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
        assert!(pubkey_data(&base).is_none());
        std::fs::write(dir.path().join("k.pub"), "only-one-field\n").unwrap();
        assert!(pubkey_data(&base).is_none());
        std::fs::write(dir.path().join("k.pub"), "ssh-rsa @@@not-base64@@@ me\n").unwrap();
        assert!(pubkey_data(&base).is_none());
        std::fs::write(dir.path().join("k.pub"), format!("{ED25519_PUB}\n")).unwrap();
        assert!(pubkey_data(&base).is_some());
    }

    /// The fixture Ed25519 key's real fingerprint, for every file-backed
    /// display below (`ssh-keygen -lf` agrees).
    const FIXTURE_FP: &str = "SHA256:lbmsoA0yIEcEiVDRnMWuzm+nV+3ZEEpVIURqFoeSspg";

    fn fixture_pub_text() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/obs/id_ed25519.pub"),
        )
        .unwrap()
    }

    #[test]
    fn file_key_fingerprint_from_dot_pub() {
        // No private file at all: the signer defers to the agent counterpart,
        // so the display names the `.pub` fingerprint.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("k");
        std::fs::write(dir.path().join("k.pub"), fixture_pub_text()).unwrap();
        assert_eq!(file_key_fingerprint(&base).as_deref(), Some(FIXTURE_FP));
    }

    /// A garbage private file beside a valid `.pub` yields `None`: only an
    /// encrypted or absent private file defers to the agent, while garbage
    /// signs fail-closed — the display must not name a fingerprint signing
    /// would never use.
    /// RED-FIRST: the old fall-through returned `Some` here.
    #[test]
    fn file_key_fingerprint_none_for_garbage_private_beside_valid_dot_pub() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("k");
        std::fs::write(&base, "not a key\n").unwrap();
        std::fs::write(dir.path().join("k.pub"), fixture_pub_text()).unwrap();
        assert_eq!(file_key_fingerprint(&base), None);
    }

    /// A non-UTF8 key name still finds its `.pub` sibling: the lookup is a
    /// path operation, not a lossy `display()` string one.
    /// RED-FIRST: the old `format!` derivation missed the sibling (`None`).
    #[cfg(unix)]
    #[test]
    fn file_key_fingerprint_finds_dot_pub_for_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(std::ffi::OsStr::from_bytes(b"key-\xff"));
        let mut pub_path = base.as_os_str().to_owned();
        pub_path.push(".pub");
        std::fs::write(&pub_path, fixture_pub_text()).unwrap();
        assert_eq!(file_key_fingerprint(&base).as_deref(), Some(FIXTURE_FP));
    }

    #[test]
    fn file_key_fingerprint_from_unencrypted_private_half() {
        // The private file alone (no `.pub` beside it): the public half still
        // yields the same fingerprint, and no secret is read for it.
        let dir = tempfile::tempdir().unwrap();
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obs/id_ed25519");
        let base = dir.path().join("id_test");
        std::fs::copy(&src, &base).unwrap();
        assert_eq!(file_key_fingerprint(&base).as_deref(), Some(FIXTURE_FP));
    }

    #[test]
    fn file_key_fingerprint_prefers_private_half_over_stale_dot_pub() {
        // A mismatched `.pub` beside a valid unencrypted key must not
        // misreport: signing uses the private half, so the display does too.
        let dir = tempfile::tempdir().unwrap();
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obs/id_ed25519");
        let base = dir.path().join("id_test");
        std::fs::copy(&src, &base).unwrap();
        std::fs::write(
            dir.path().join("id_test.pub"),
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBOhp76L4G35dZCjy2rSLa8dOnzQebZz4eiiC4zCWEv3OnX8XoSamKF6l73YZtDt5wo6VYanto8+l0TC/750dsdI= other\n",
        )
        .unwrap();
        assert_eq!(file_key_fingerprint(&base).as_deref(), Some(FIXTURE_FP));
    }

    #[test]
    fn file_key_fingerprint_none_for_missing_or_garbage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(file_key_fingerprint(&dir.path().join("absent")), None);
        let base = dir.path().join("k");
        std::fs::write(&base, "not a key\n").unwrap();
        std::fs::write(dir.path().join("k.pub"), "only-one-field\n").unwrap();
        assert_eq!(file_key_fingerprint(&base), None);
    }

    #[test]
    fn key_display_prefers_configured_then_file_then_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let src =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obs/id_ed25519");
        let base = dir.path().join("id_test");
        std::fs::copy(&src, &base).unwrap();
        assert_eq!(key_display(Some(&base), Some("SHA256:abc")), "SHA256:abc");
        assert_eq!(key_display(Some(&base), Some("abc")), "SHA256:abc");
        assert_eq!(key_display(Some(&base), None), FIXTURE_FP);
        assert_eq!(
            key_display(Some(dir.path().join("absent").as_path()), None),
            "file key"
        );
        assert_eq!(key_display(None, None), "file key");
    }

    #[test]
    fn no_agent_key_message_names_context() {
        let e = SshSigError::NoAgentKey {
            fingerprint: "SHA256:xyz".to_owned(),
            context: "teregen auth",
        };
        assert!(e.to_string().contains("no key matching fingerprint"));
        assert!(e.to_string().contains("teregen auth never prompts"));
    }

    #[test]
    fn passphrase_protected_message_names_context_and_hint() {
        let e = SshSigError::PassphraseProtected {
            path: PathBuf::from("/x/id_rsa"),
            hint_available: false,
            context: "teregen auth",
        };
        let msg = e.to_string();
        assert!(msg.contains("passphrase-protected"));
        assert!(msg.contains("no /x/id_rsa.pub found"));
        assert!(msg.contains("teregen auth never prompts"));
    }

    #[test]
    fn encode_bare_and_pem_differ_in_armor() {
        let key = PrivateKey::random(&mut rand::rng(), ssh_key::Algorithm::Ed25519).unwrap();
        let sig = sign_message(&key, "test-namespace", b"hello").unwrap();
        let bare = encode_bare(&sig).unwrap();
        let pem = encode_pem(&sig).unwrap();
        assert!(!bare.contains("BEGIN SSH SIGNATURE"));
        assert!(pem.contains("-----BEGIN SSH SIGNATURE-----"));
        assert!(pem.contains(&bare.chars().take(20).collect::<String>()));
    }

    /// A backend failure carrying a socket path surfaces as `Agent(String)`:
    /// the `Signer` maps `AgentKeys` errors without redaction (see the three
    /// `SshSigError::Agent(e.to_string())` sites). `Signer` is shared by the
    /// headless writes too, so this pins only the variant shape, never the
    /// verbatim content — a path-echoing backend must not gain a redaction
    /// contract from this test.
    /// RED-FIRST: asserting the variant fails if the mapping ever folds
    /// backend failures into another variant.
    struct SocketLeakingAgent {
        sock: String,
    }

    #[async_trait::async_trait]
    impl AgentKeys for SocketLeakingAgent {
        async fn identities(&mut self) -> Result<Vec<PublicKey>, crate::obs::errors::ObsError> {
            Err(crate::obs::errors::ObsError::Config(format!(
                "could not query the ssh-agent: connect {}: connection refused",
                self.sock
            )))
        }

        async fn sign(
            &mut self,
            _public: &PublicKey,
            _hash_alg: Option<HashAlg>,
            _data: &[u8],
        ) -> Result<Signature, crate::obs::errors::ObsError> {
            Err(crate::obs::errors::ObsError::Config(format!(
                "ssh-agent signing failed: connect {}: connection refused",
                self.sock
            )))
        }
    }

    #[tokio::test]
    async fn agent_backend_failure_surfaces_as_agent() {
        let signer = Signer::with_agent(
            None,
            Some("SHA256:abc123".to_owned()),
            "teregen auth",
            SocketLeakingAgent {
                sock: "/tmp/agent-sock".to_owned(),
            },
        );
        let err = signer.sign("teregen-auth", b"nonce").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, SshSigError::Agent(_)),
            "backend failures surface as Agent: {msg:?}"
        );
    }
}
