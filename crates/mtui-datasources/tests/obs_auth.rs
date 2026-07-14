//! Integration tests for OBS SSH-signature auth
//! (`mtui_datasources::obs::auth`).
//!
//! Ports the behavioral core of upstream `tests/test_obs_auth.py`: the
//! retry-once 401 Signature flow through the real transport (`wiremock`), the
//! "no Signature scheme → no retry" / "non-401 passes through" branches, and
//! the ssh-agent selection paths (by SHA256 fingerprint, by `.pub`-matched
//! counterpart, `.pub`-only on disk) plus their fail-closed cases — all offline
//! via a mock [`AgentKeys`] (no live ssh-agent). Also asserts the Authorization
//! header/signature is never logged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mtui_datasources::VerifyPolicy;
use mtui_datasources::obs::{AgentKeys, ObsClient, ObsError, ObsSignatureAuth};
use ssh_key::{HashAlg, PrivateKey, PublicKey, Signature};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REALM: &str = "Use your developer account";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/obs")
        .join(name)
}

/// A mock ssh-agent holding a set of decrypted private keys, signing in-process.
///
/// Reproduces a real agent's behavior (raw signature bytes, RSA→rsa-sha2-512 via
/// the `hash_alg` flag) without a socket, so every agent-selection branch runs
/// offline. A regression that mishandled the agent signature packing would fail
/// the `_assert_authz_verifies` round-trip here.
struct MockAgent {
    keys: Vec<PrivateKey>,
    fail: bool,
}

impl MockAgent {
    fn with(keys: Vec<PrivateKey>) -> Self {
        Self { keys, fail: false }
    }
    fn failing() -> Self {
        Self {
            keys: vec![],
            fail: true,
        }
    }
}

#[async_trait::async_trait]
impl AgentKeys for MockAgent {
    async fn identities(&mut self) -> Result<Vec<PublicKey>, ObsError> {
        if self.fail {
            return Err(ObsError::Config(
                "could not query the ssh-agent: boom".to_owned(),
            ));
        }
        Ok(self.keys.iter().map(|k| k.public_key().clone()).collect())
    }

    async fn sign(
        &mut self,
        public: &PublicKey,
        _hash_alg: Option<HashAlg>,
        data: &[u8],
    ) -> Result<Signature, ObsError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.public_key().key_data() == public.key_data())
            .expect("agent asked to sign with a key it holds");
        Ok(signature::Signer::sign(key, data))
    }
}

fn auth_with_agent(
    sshkey_path: Option<PathBuf>,
    sshkey_fingerprint: Option<String>,
    agent: MockAgent,
) -> ObsSignatureAuth<MockAgent> {
    ObsSignatureAuth::with_agent("qamuser".to_owned(), sshkey_path, sshkey_fingerprint, agent)
}

/// Verify the Authorization header's embedded SSHSIG against `signer`'s key.
fn assert_authz_verifies(signer: &PrivateKey, authz: &str) {
    use base64ct::{Base64, Encoding as _};
    use mtui_datasources::obs::sshsig;
    use ssh_key::SshSig;
    use ssh_key::encoding::Decode;

    let b64 = authz
        .split("signature=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let created: i64 = authz
        .split("created=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = Base64::decode_vec(b64).unwrap();
    let sig = SshSig::decode(&mut bytes.as_slice()).unwrap();
    assert_eq!(sig.public_key(), signer.public_key().key_data());
    assert_eq!(sig.hash_alg(), HashAlg::Sha512);
    let msg = sshsig::created_message(created);
    signer
        .public_key()
        .verify(REALM, &msg, &sig)
        .expect("Authorization SSHSIG verifies");
}

// ---- 401 challenge/response through the real transport --------------------

#[tokio::test]
async fn signs_and_retries_once_on_401() {
    let server = MockServer::start().await;
    // First GET (unauthenticated) → 401 Signature challenge.
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(ResponseTemplate::new(401).append_header(
            "WWW-Authenticate",
            format!("Signature realm=\"{REALM}\"").as_str(),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Second GET (signed) → 200.
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&server)
        .await;

    let auth = ObsSignatureAuth::new("qamuser".to_owned(), Some(fixture("id_ed25519")), None);
    let client = ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(auth),
    )
    .unwrap();

    let body = client
        .get("request/1", &[])
        .await
        .expect("signed retry succeeds");
    assert_eq!(body, "<ok/>");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2, "exactly one retry");
    assert!(
        reqs[0].headers.get("Authorization").is_none(),
        "first request is unauthenticated"
    );
    let authz = reqs[1]
        .headers
        .get("Authorization")
        .expect("second request carries Authorization")
        .to_str()
        .unwrap();
    assert!(authz.starts_with("Signature keyId=\"qamuser\",algorithm=\"ssh\""));
    assert!(authz.contains("headers=\"(created)\""));
    assert!(authz.contains("created="));
    assert!(authz.contains("signature=\""));
}

#[tokio::test]
async fn no_signature_scheme_does_not_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(
            ResponseTemplate::new(401).append_header("WWW-Authenticate", "Basic realm=\"x\""),
        )
        .mount(&server)
        .await;

    let auth = ObsSignatureAuth::new("qamuser".to_owned(), Some(fixture("id_ed25519")), None);
    let client = ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(auth),
    )
    .unwrap();

    let err = client
        .get("request/1", &[])
        .await
        .expect_err("401 stays an error");
    assert!(
        matches!(err, ObsError::Api { status: 401, .. }),
        "got {err:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "no retry"
    );
}

#[tokio::test]
async fn non_401_passes_through() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&server)
        .await;

    let auth = ObsSignatureAuth::new("qamuser".to_owned(), Some(fixture("id_ed25519")), None);
    let client = ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(auth),
    )
    .unwrap();

    assert_eq!(client.get("request/1", &[]).await.unwrap(), "<ok/>");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// ---- key-file signing -----------------------------------------------------

#[tokio::test]
async fn rsa_and_ecdsa_files_sign() {
    for name in ["id_rsa", "id_ecdsa"] {
        let auth = ObsSignatureAuth::new("qamuser".to_owned(), Some(fixture(name)), None);
        let authz = auth.authorization(REALM).await.expect("file key signs");
        assert!(
            authz.contains("signature=\""),
            "{name} produced a signature"
        );
    }
}

// ---- ssh-agent selection --------------------------------------------------

#[tokio::test]
async fn agent_fingerprint_signs_with_and_without_prefix() {
    let signer = PrivateKey::read_openssh_file(fixture("id_ecdsa")).unwrap();
    let fp = signer.public_key().fingerprint(HashAlg::Sha256).to_string();
    let bare = fp.split_once(':').unwrap().1.to_owned();

    for locator in [fp.clone(), bare] {
        let agent = MockAgent::with(vec![signer.clone()]);
        let auth = auth_with_agent(None, Some(locator), agent);
        let authz = auth.authorization(REALM).await.expect("agent signs");
        assert_authz_verifies(&signer, &authz);
    }
}

#[tokio::test]
async fn agent_fingerprint_not_found_fails_closed() {
    let other = PrivateKey::read_openssh_file(fixture("id_ecdsa")).unwrap();
    let agent = MockAgent::with(vec![other]);
    let auth = auth_with_agent(None, Some("SHA256:missing".to_owned()), agent);
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("missing fp fails");
    assert!(matches!(err, ObsError::Config(m) if m.contains("no key matching fingerprint")));
}

#[tokio::test]
async fn agent_query_error_fails_closed() {
    let auth = auth_with_agent(None, Some("SHA256:x".to_owned()), MockAgent::failing());
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("agent error fails");
    assert!(matches!(err, ObsError::Config(m) if m.contains("ssh-agent")));
}

#[tokio::test]
async fn encrypted_key_falls_back_to_agent() {
    // An encrypted private key on disk whose .pub identifies the agent's key.
    let dir = tempfile::tempdir().unwrap();
    let signer = PrivateKey::read_openssh_file(fixture("id_rsa")).unwrap();

    let enc = signer.encrypt(&mut rand::rng(), "hunter2").unwrap();
    let priv_path = dir.path().join("id_rsa");
    enc.write_openssh_file(&priv_path, ssh_key::LineEnding::LF)
        .unwrap();
    std::fs::write(
        dir.path().join("id_rsa.pub"),
        signer.public_key().to_openssh().unwrap(),
    )
    .unwrap();

    let agent = MockAgent::with(vec![signer.clone()]);
    let auth = auth_with_agent(Some(priv_path), None, agent);
    let authz = auth.authorization(REALM).await.expect("encrypted → agent");
    assert_authz_verifies(&signer, &authz);
}

#[tokio::test]
async fn pub_only_file_uses_agent() {
    // Only <name>.pub exists on disk; the private half lives in the agent.
    let dir = tempfile::tempdir().unwrap();
    let signer = PrivateKey::read_openssh_file(fixture("id_rsa")).unwrap();
    let priv_path = dir.path().join("id_rsa"); // does not exist
    std::fs::write(
        dir.path().join("id_rsa.pub"),
        signer.public_key().to_openssh().unwrap(),
    )
    .unwrap();

    let agent = MockAgent::with(vec![signer.clone()]);
    let auth = auth_with_agent(Some(priv_path), None, agent);
    let authz = auth.authorization(REALM).await.expect("pub-only → agent");
    assert_authz_verifies(&signer, &authz);
}

#[tokio::test]
async fn missing_file_no_pub_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let auth = auth_with_agent(
        Some(dir.path().join("absent")),
        None,
        MockAgent::with(vec![]),
    );
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("no key resolves");
    assert!(matches!(err, ObsError::Config(m) if m.contains("passphrase-protected")));
}

#[tokio::test]
async fn encrypted_key_pub_not_in_agent_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let signer = PrivateKey::read_openssh_file(fixture("id_rsa")).unwrap();
    let enc = signer.encrypt(&mut rand::rng(), "hunter2").unwrap();
    let priv_path = dir.path().join("id_rsa");
    enc.write_openssh_file(&priv_path, ssh_key::LineEnding::LF)
        .unwrap();
    std::fs::write(
        dir.path().join("id_rsa.pub"),
        signer.public_key().to_openssh().unwrap(),
    )
    .unwrap();

    // The agent holds a *different* key.
    let other = PrivateKey::read_openssh_file(fixture("id_ecdsa")).unwrap();
    let auth = auth_with_agent(Some(priv_path), None, MockAgent::with(vec![other]));
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("mismatched agent fails");
    assert!(matches!(err, ObsError::Config(m) if m.contains("passphrase-protected")));
}

#[tokio::test]
async fn authorization_and_signature_are_never_logged() {
    use std::sync::{Arc as StdArc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct BufMaker(StdArc<Mutex<Vec<u8>>>);
    struct BufWriter(StdArc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for BufMaker {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            BufWriter(self.0.clone())
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(ResponseTemplate::new(401).append_header(
            "WWW-Authenticate",
            format!("Signature realm=\"{REALM}\"").as_str(),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&server)
        .await;

    let buf = StdArc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(BufMaker(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(sub);

    // Capture the header we produced, then run the signed request under the
    // subscriber; the header/signature must appear in neither the logs.
    let auth = ObsSignatureAuth::new("qamuser".to_owned(), Some(fixture("id_ed25519")), None);
    let header = auth.authorization(REALM).await.unwrap();
    let signature = header.split("signature=\"").nth(1).unwrap();

    let client = ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(ObsSignatureAuth::new(
            "qamuser".to_owned(),
            Some(fixture("id_ed25519")),
            None,
        )),
    )
    .unwrap();
    client.get("request/1", &[]).await.unwrap();

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.contains(&signature[..40]),
        "signature leaked into logs: {logs}"
    );
    assert!(
        !logs.contains("Authorization"),
        "Authorization header name leaked into logs: {logs}"
    );
}

#[tokio::test]
async fn unusable_key_file_fails_closed() {
    // A binary/non-key file is not loadable and has no .pub → typed error.
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("id_bin");
    std::fs::write(&bad, (0u8..=255).collect::<Vec<_>>()).unwrap();
    let auth = auth_with_agent(Some(bad), None, MockAgent::with(vec![]));
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("binary key fails");
    // A non-OpenSSH file surfaces as "not a usable private key" or, if it looks
    // absent to the loader, the passphrase-protected/agent path — both Config.
    assert!(matches!(err, ObsError::Config(_)), "got {err:?}");
}

#[tokio::test]
async fn unreadable_key_dir_fails_closed() {
    // A key path that cannot be read (here a directory) fails closed with a
    // typed error rather than a panic (upstream `test_unreadable_key_fails_closed`).
    let dir = tempfile::tempdir().unwrap();
    let keydir = dir.path().join("keydir");
    std::fs::create_dir(&keydir).unwrap();
    let auth = auth_with_agent(Some(keydir), None, MockAgent::with(vec![]));
    let err = auth
        .authorization(REALM)
        .await
        .expect_err("directory key fails");
    assert!(matches!(err, ObsError::Config(_)), "got {err:?}");
}
