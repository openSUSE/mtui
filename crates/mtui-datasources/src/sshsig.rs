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
    let pub_path = format!("{}.pub", path.display());
    let text = std::fs::read_to_string(pub_path).ok()?;
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
}
