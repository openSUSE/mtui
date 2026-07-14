//! OpenSSH SSHSIG wire-format signer for OBS "Signature" auth.
//!
//! Ported from upstream `mtui/data_sources/obs/sshsig.py`. Reproduces
//! `ssh-keygen -Y sign` for any key type (Ed25519, ECDSA, RSA), so mtui
//! authenticates to the OBS API in-process — no `osc` library and no signing
//! subprocess. The format is OpenSSH's SSHSIG ([PROTOCOL.sshsig]): the message
//! `(created): <epoch>` is hashed with `sha512`, wrapped with the namespace
//! (the challenge realm), signed, and the signature plus public key are packed
//! into the outer SSHSIG blob, returned **base64-encoded WITHOUT the PEM
//! armor** — that raw base64 is what the OBS `Authorization: Signature` header
//! carries.
//!
//! RSA keys are signed with `rsa-sha2-512` (`ssh-key`'s default RSA `Signer`
//! impl), not the legacy `ssh-rsa` (SHA-1) that modern OpenSSH/OBS reject.
//! Ed25519 and ECDSA have a single signature
//! algorithm. The message-hash algorithm in the SSHSIG blob (`sha512`) is
//! independent of the signature algorithm and stays fixed, matching
//! `ssh-keygen -Y sign`.
//!
//! Two call paths feed one packer: a file-backed [`PrivateKey`] via
//! [`sign_created`], and an ssh-agent key via [`agent_signed_data`] +
//! [`pack_agent_signature`] (whose raw signature bytes are produced by
//! [`crate::obs::auth`] over the ssh-agent). Both yield the identical outer
//! blob (asserted byte-for-byte in `tests/obs_sshsig.rs`).
//!
//! [PROTOCOL.sshsig]: https://cvsweb.openbsd.org/src/usr.bin/ssh/PROTOCOL.sshsig

use base64ct::{Base64, Encoding as _};
use ssh_key::encoding::Encode;
use ssh_key::public::KeyData;
use ssh_key::{HashAlg, PrivateKey, Signature, SshSig};

use crate::obs::errors::ObsError;

/// The fixed SSHSIG message-hash algorithm, matching `ssh-keygen -Y sign`.
const HASH_ALG: HashAlg = HashAlg::Sha512;

/// The message signed under the SSHSIG envelope: OBS's `(created): <epoch>`.
///
/// Kept as a helper so the file-key path (which hashes internally) and the
/// agent path (which pre-builds the enveloped bytes to hand to the agent) agree
/// byte-for-byte.
#[must_use]
pub fn created_message(created: i64) -> Vec<u8> {
    format!("(created): {created}").into_bytes()
}

/// Build the base64 SSHSIG over the OBS `(created): <epoch>` payload using a
/// file-backed private key of any supported type.
///
/// `namespace` is the SSHSIG namespace — the challenge `realm` (the live
/// api.suse.de value is `Use your developer account`). `created` is the signed
/// Unix timestamp, also sent as the Authorization `created` field.
///
/// Returns the base64-encoded outer SSHSIG blob (no PEM armor) for the
/// Authorization `signature` field.
///
/// # Errors
///
/// Returns [`ObsError::Config`] if the key's algorithm is unsupported or the
/// cryptographic backend fails (fail-closed; the message never leaks key
/// material).
pub fn sign_created(key: &PrivateKey, namespace: &str, created: i64) -> Result<String, ObsError> {
    let msg = created_message(created);
    let sig = SshSig::sign(key, namespace, HASH_ALG, &msg)
        .map_err(|e| ObsError::Config(format!("could not sign OBS challenge: {e}")))?;
    encode(&sig)
}

/// The pre-hashed enveloped bytes the ssh-agent must sign for the agent path.
///
/// The agent signs raw bytes, so [`crate::obs::auth`] asks it to sign exactly
/// [`SshSig::signed_data`] (`MAGIC | namespace | reserved | hash_alg |
/// H(message)`), then hands the resulting [`Signature`] plus the agent key's
/// public data to [`pack_agent_signature`].
///
/// # Errors
///
/// Returns [`ObsError::Config`] if the namespace is empty (SSHSIG forbids it).
pub fn agent_signed_data(namespace: &str, created: i64) -> Result<Vec<u8>, ObsError> {
    let msg = created_message(created);
    SshSig::signed_data(namespace, HASH_ALG, &msg)
        .map_err(|e| ObsError::Config(format!("could not build OBS challenge: {e}")))
}

/// Pack an ssh-agent's raw [`Signature`] over [`agent_signed_data`] into the
/// base64 outer SSHSIG blob.
///
/// # Errors
///
/// Returns [`ObsError::Config`] if the public key / namespace / signature
/// cannot be assembled into a valid SSHSIG (fail-closed).
pub fn pack_agent_signature(
    public_key: &KeyData,
    namespace: &str,
    signature: Signature,
) -> Result<String, ObsError> {
    let sig = SshSig::new(public_key.clone(), namespace, HASH_ALG, signature)
        .map_err(|e| ObsError::Config(format!("could not assemble OBS signature: {e}")))?;
    encode(&sig)
}

/// Base64-encode the outer SSHSIG wire blob (no PEM armor).
fn encode(sig: &SshSig) -> Result<String, ObsError> {
    let mut bytes = Vec::new();
    sig.encode(&mut bytes)
        .map_err(|e| ObsError::Config(format!("could not encode OBS signature: {e}")))?;
    Ok(Base64::encode_string(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_message_matches_upstream_payload() {
        assert_eq!(created_message(1_700_000_000), b"(created): 1700000000");
    }
}
