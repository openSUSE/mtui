//! OpenSSH SSHSIG wire-format signer for OBS "Signature" auth.
//!
//! Thin wrappers over [`crate::sshsig`]'s message-generic signer, fixing the
//! encoding OBS expects (raw base64, no PEM armor) and the message OBS signs
//! (`(created): <epoch>`). See [`crate::sshsig`] for the shared mechanics; this
//! module exists only to keep the four OBS entry points' names and signatures
//! stable for [`crate::obs::auth`].
//!
//! [PROTOCOL.sshsig]: https://cvsweb.openbsd.org/src/usr.bin/ssh/PROTOCOL.sshsig

use ssh_key::public::KeyData;
use ssh_key::{PrivateKey, Signature};

use crate::obs::errors::ObsError;

/// The message signed under the SSHSIG envelope: OBS's `(created): <epoch>`.
///
/// A helper so the file-key path (which hashes internally) and the agent path
/// (which pre-builds the enveloped bytes) agree byte-for-byte.
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
/// crypto backend fails; fail-closed, and the message never leaks key material.
pub fn sign_created(key: &PrivateKey, namespace: &str, created: i64) -> Result<String, ObsError> {
    let msg = created_message(created);
    let sig = crate::sshsig::sign_message(key, namespace, &msg)?;
    Ok(crate::sshsig::encode_bare(&sig)?)
}

/// The pre-hashed enveloped bytes the ssh-agent must sign for the agent path.
///
/// The agent signs raw bytes, so [`crate::obs::auth`] asks it to sign exactly
/// [`crate::sshsig::signed_data`] (`MAGIC | namespace | reserved | hash_alg |
/// H(message)`), then hands the resulting [`Signature`] plus the agent key's
/// public data to [`pack_agent_signature`].
///
/// # Errors
///
/// Returns [`ObsError::Config`] if the namespace is empty (SSHSIG forbids it).
pub fn agent_signed_data(namespace: &str, created: i64) -> Result<Vec<u8>, ObsError> {
    let msg = created_message(created);
    Ok(crate::sshsig::signed_data(namespace, &msg)?)
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
    let sig = crate::sshsig::pack(public_key, namespace, signature)?;
    Ok(crate::sshsig::encode_bare(&sig)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_message_has_stable_payload() {
        assert_eq!(created_message(1_700_000_000), b"(created): 1700000000");
    }
}
