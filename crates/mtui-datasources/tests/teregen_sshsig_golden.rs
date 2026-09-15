//! Pins the teregen wire shape: mtui's armored SSHSIG must be byte-identical
//! to a real `ssh-keygen -Y sign -n teregen-auth` golden.
//!
//! `tests/fixtures/teregen/nonce.sig` was produced with the real binary:
//!
//! ```sh
//! printf '%s' "$NONCE" | ssh-keygen -Y sign -n teregen-auth \
//!     -f tests/fixtures/obs/id_ed25519 -
//! ```
//!
//! Ed25519 SSHSIG is deterministic, so equality is a total check of namespace +
//! message framing + armor, offline and always able to fail — the same
//! precedent as `obs_sshsig::ed25519_matches_ssh_keygen_sig_file`.

use mtui_datasources::sshsig;
use ssh_key::PrivateKey;

const NAMESPACE: &str = "teregen-auth";

/// The fixed 64-hex nonce the golden was signed over.
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn fixture(dir: &str, name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

fn load_key() -> PrivateKey {
    PrivateKey::read_openssh_file(fixture("obs", "id_ed25519")).expect("fixture private key loads")
}

fn golden() -> String {
    std::fs::read_to_string(fixture("teregen", "nonce.sig")).expect("golden .sig file")
}

#[test]
fn encode_pem_matches_ssh_keygen_golden_byte_for_byte() {
    let key = load_key();
    let sig = sshsig::sign_message(&key, NAMESPACE, NONCE.as_bytes()).unwrap();
    let pem = sshsig::encode_pem(&sig).unwrap();
    assert_eq!(pem, golden());
}

/// Mutation 1: swapping the armored encoding for the bare one must not match
/// the PEM golden.
#[test]
fn encode_bare_does_not_match_the_pem_golden() {
    let key = load_key();
    let sig = sshsig::sign_message(&key, NAMESPACE, NONCE.as_bytes()).unwrap();
    let bare = sshsig::encode_bare(&sig).unwrap();
    assert_ne!(bare, golden());
}

/// Mutation 2: a trailing newline on the signed message must change the
/// signature (SSHSIG signs the bare nonce bytes, no trailing newline).
#[test]
fn trailing_newline_on_message_changes_the_signature() {
    let key = load_key();
    let with_newline = format!("{NONCE}\n");
    let sig = sshsig::sign_message(&key, NAMESPACE, with_newline.as_bytes()).unwrap();
    let pem = sshsig::encode_pem(&sig).unwrap();
    assert_ne!(pem, golden());
}

/// Mutation 3: a different namespace must change the signature.
#[test]
fn wrong_namespace_changes_the_signature() {
    let key = load_key();
    let sig = sshsig::sign_message(&key, "wrong-namespace", NONCE.as_bytes()).unwrap();
    let pem = sshsig::encode_pem(&sig).unwrap();
    assert_ne!(pem, golden());
}
