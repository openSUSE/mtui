//! Golden-vector tests for the OBS SSHSIG signer
//! (`mtui_datasources::obs::sshsig`).
//!
//! The signer must reproduce `ssh-keygen -Y sign` because the base64 SSHSIG
//! blob goes straight into the OBS `Authorization: Signature` header. The
//! fixtures under `tests/fixtures/obs/` are committed goldens:
//!
//! * `id_ed25519` — derived from the fixed 32-byte seed upstream uses
//!   (`test_obs_sshsig.py`), so its signature is deterministic and equals
//!   upstream's committed `GOLDEN` byte-for-byte.
//! * `id_rsa` / `id_ecdsa` — generated once with `ssh-keygen`; their `.sig`
//!   files are `ssh-keygen -Y sign` output over the same `(created)` payload.
//!
//! Each `<key>.sig` is a PEM-armored SSHSIG; the signer emits the same blob as
//! raw base64 (no armor), so the golden is the PEM body with the armor stripped.
//! Ed25519 and RSA (pkcs1v15) signing are deterministic → exact byte match.
//! ECDSA is randomised → verify the blob round-trips instead.

use ssh_key::{HashAlg, PrivateKey, PublicKey, SshSig};

use mtui_datasources::obs::sshsig;

const NAMESPACE: &str = "Use your developer account";
const CREATED: i64 = 1_700_000_000;

/// Upstream's committed Ed25519 golden (`test_obs_sshsig.py::GOLDEN`).
const ED25519_GOLDEN: &str = concat!(
    "U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgA6EHv/POEL4dcN0Y50vAmWfk1jCb",
    "pQ1fHdyGZBJVMbgAAAAaVXNlIHlvdXIgZGV2ZWxvcGVyIGFjY291bnQAAAAAAAAABnNoYTUx",
    "MgAAAFMAAAALc3NoLWVkMjU1MTkAAABAiD/Mxc0SAyrM6wVmT1T9BF7dNbv0dUKD4i3Tmxwq",
    "iXyrstfeqgcwEx6QkzfDwCjMNPD97uiIcGJjfR2yLhtBCw==",
);

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/obs")
        .join(name)
}

fn load_key(name: &str) -> PrivateKey {
    PrivateKey::read_openssh_file(fixture(name)).expect("fixture private key loads")
}

/// The raw base64 SSHSIG body of a committed `ssh-keygen -Y sign` `.sig` file
/// (PEM armor stripped) — the golden the signer must reproduce.
fn golden_from_sig(name: &str) -> String {
    let pem = std::fs::read_to_string(fixture(name)).expect("golden .sig file");
    pem.lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<String>()
}

#[test]
fn ed25519_matches_upstream_committed_golden() {
    // The fixture key is byte-identical to upstream's seed-derived key, so the
    // deterministic Ed25519 signature equals the committed GOLDEN exactly.
    let key = load_key("id_ed25519");
    assert_eq!(
        sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap(),
        ED25519_GOLDEN
    );
}

#[test]
fn ed25519_matches_ssh_keygen_sig_file() {
    // Defence in depth: also equals the checked-in `ssh-keygen -Y sign` output.
    let key = load_key("id_ed25519");
    assert_eq!(
        sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap(),
        golden_from_sig("id_ed25519.sig")
    );
}

#[test]
fn rsa_matches_ssh_keygen_sig_file_with_rsa_sha2_512() {
    // RSA pkcs1v15 signing is deterministic, so the blob equals ssh-keygen's.
    let key = load_key("id_rsa");
    let blob = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();
    assert_eq!(blob, golden_from_sig("id_rsa.sig"));

    // And the inner signature algorithm must be rsa-sha2-512 (not ssh-rsa/SHA-1).
    let sig = decode_sshsig(&blob);
    assert_eq!(sig.signature().algorithm().to_string(), "rsa-sha2-512");
}

#[test]
fn ecdsa_blob_round_trips_and_verifies() {
    // ECDSA signing is randomised, so byte-match is impossible; instead verify
    // the produced blob is a valid SSHSIG over the payload for the key.
    let key = load_key("id_ecdsa");
    let blob = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();
    let sig = decode_sshsig(&blob);

    assert_eq!(sig.namespace(), NAMESPACE);
    assert_eq!(sig.hash_alg(), HashAlg::Sha512);
    assert_eq!(sig.public_key(), key.public_key().key_data());
    assert_eq!(
        sig.signature().algorithm().to_string(),
        "ecdsa-sha2-nistp256"
    );

    let msg = sshsig::created_message(CREATED);
    key.public_key()
        .verify(NAMESPACE, &msg, &sig)
        .expect("ECDSA SSHSIG verifies");
}

#[test]
fn ed25519_is_deterministic() {
    let key = load_key("id_ed25519");
    let a = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();
    let b = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();
    assert_eq!(a, b);
}

#[test]
fn created_and_namespace_change_the_signature() {
    let key = load_key("id_ed25519");
    let base = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();
    assert_ne!(
        base,
        sshsig::sign_created(&key, NAMESPACE, CREATED + 1).unwrap()
    );
    assert_ne!(
        base,
        sshsig::sign_created(&key, "other namespace", CREATED).unwrap()
    );
}

#[test]
fn agent_path_packs_the_same_blob_as_the_file_path() {
    // The agent path signs `agent_signed_data` and packs via
    // `pack_agent_signature`; for a deterministic key this must equal the
    // file-path blob, proving the two backends agree byte-for-byte.
    let key = load_key("id_ed25519");
    let file_blob = sshsig::sign_created(&key, NAMESPACE, CREATED).unwrap();

    let signed_data = sshsig::agent_signed_data(NAMESPACE, CREATED).unwrap();
    let signature = signature::Signer::sign(&key, &signed_data);
    let agent_blob =
        sshsig::pack_agent_signature(key.public_key().key_data(), NAMESPACE, signature).unwrap();

    assert_eq!(file_blob, agent_blob);
}

/// Decode a raw (armorless) base64 SSHSIG blob back into an [`SshSig`].
fn decode_sshsig(blob_b64: &str) -> SshSig {
    use base64ct::{Base64, Encoding as _};
    use ssh_key::encoding::Decode;
    let bytes = Base64::decode_vec(blob_b64).expect("valid base64");
    SshSig::decode(&mut bytes.as_slice()).expect("valid SSHSIG blob")
}

/// A pub-key helper used to assert the fixtures line up (not strictly needed,
/// but documents the expected algorithms).
#[test]
fn fixtures_cover_all_three_algorithms() {
    let algos: Vec<String> = ["id_ed25519.pub", "id_rsa.pub", "id_ecdsa.pub"]
        .iter()
        .map(|f| {
            PublicKey::read_openssh_file(fixture(f))
                .unwrap()
                .algorithm()
                .to_string()
        })
        .collect();
    assert!(algos.iter().any(|a| a == "ssh-ed25519"));
    assert!(algos.iter().any(|a| a == "ssh-rsa"));
    assert!(algos.iter().any(|a| a.starts_with("ecdsa-sha2-")));
}
