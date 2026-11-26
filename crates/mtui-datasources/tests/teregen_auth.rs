//! Wiremock matrix for teregen SSH-signature auth
//! (`mtui_datasources::teregen::auth`).
//!
//! Covers the challenge/verify happy path, the armor regression (teregen
//! expects PEM, not OBS's raw base64), the rate-limit/protocol/malformed-nonce
//! fast-fail paths (never touching `/verify`), the indistinguishable-401 hint,
//! a missing-token response, the cache hit/miss behaviour, and the P2-D7
//! secrets discipline (never logged, never `Debug`-printed).
//!
//! The re-mint-once-retry-once authenticated-request helper (P2-D6) has its
//! own cases once it exists (Step 7).

use std::path::{Path, PathBuf};

use mtui_datasources::teregen::{AuthHint, AuthStage, TeregenAuth, TeregenAuthError, TokenStore};
use mtui_datasources::{HttpClient, VerifyPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::log_capture::capture_logs;
use crate::mock_agent::MockAgent;

/// The fixed 64-hex nonce every "challenge 200" mock returns.
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PRINCIPAL: &str = "alice";

fn fixture(dir: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

/// Build a [`TeregenAuth`] pointed at `server`, caching to a fresh tempdir
/// file, signing with the deterministic Ed25519 fixture key (unencrypted, so
/// the file-key path never touches an agent).
fn auth_for(server: &MockServer, store_path: PathBuf) -> TeregenAuth {
    TeregenAuth::new(
        server.uri(),
        PRINCIPAL.to_owned(),
        Some(fixture("obs", "id_ed25519")),
        None,
        HttpClient::new(VerifyPolicy::Default(true)).expect("client builds"),
    )
    .with_store(Some(TokenStore::at(store_path)))
}

fn store_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let store_file = dir.path().join("teregen-token.json");
    (dir, store_file)
}

async fn mount_challenge_200(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": NONCE})))
        .mount(server)
        .await;
}

/// 1. challenge 200 → verify 200: token returned and stored with the right
///    `base`/`principal`.
#[tokio::test]
async fn challenge_then_verify_success_stores_the_token() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "deadbeef".repeat(8),
        })))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file.clone());
    let token = auth.token().await.expect("mints a token");
    assert_eq!(token, "deadbeef".repeat(8));

    let store = TokenStore::at(store_file);
    let cached = store
        .load(&server.uri(), PRINCIPAL)
        .expect("token was cached");
    assert_eq!(cached.token, token);
}

/// 2. The armor regression: `signature` is PEM-armored (not OBS's raw
///    base64), `nonce` is echoed verbatim, `username` matches the principal.
#[tokio::test]
async fn verify_request_body_carries_the_pem_armored_signature() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    auth.token().await.expect("mints a token");

    let requests = server.received_requests().await.unwrap();
    let verify_req = requests
        .iter()
        .find(|r| r.url.path() == "/auth/ssh/verify")
        .expect("a verify request was made");
    let body: serde_json::Value = verify_req.body_json().unwrap();

    assert_eq!(body["username"], PRINCIPAL);
    assert_eq!(body["nonce"], NONCE);
    let sig = body["signature"].as_str().expect("signature is a string");
    assert!(
        sig.starts_with("-----BEGIN SSH SIGNATURE-----"),
        "not armored: {sig}"
    );
    assert!(
        sig.trim_end().ends_with("-----END SSH SIGNATURE-----"),
        "missing footer: {sig}"
    );
}

/// 3. challenge 429 → `RateLimited`; `/verify` is never called.
#[tokio::test]
async fn challenge_rate_limited_never_calls_verify() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(serde_json::json!({"error": "too_many_requests"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let err = auth.token().await.expect_err("rate limited");
    assert!(matches!(err, TeregenAuthError::RateLimited));
}

/// 4. challenge 400 → `Protocol`; `/verify` is never called.
#[tokio::test]
async fn challenge_400_is_protocol_error() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let err = auth.token().await.expect_err("400 refused");
    assert!(matches!(err, TeregenAuthError::Protocol(_)));
}

/// 5. challenge 200 with a non-64-hex nonce fails before signing; `/verify` is
///    never called.
#[tokio::test]
async fn malformed_nonce_fails_before_signing() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": "not-hex"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let err = auth.token().await.expect_err("malformed nonce");
    assert!(matches!(err, TeregenAuthError::Protocol(_)));
}

/// 6. verify 401 → `Unauthorized` naming the principal, the fingerprint of
///    the key that actually signed, the server and the stage, with a hint
///    naming the indistinguishable causes.
#[tokio::test]
async fn verify_401_is_unauthorized_with_a_hint() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "unauthorized"})),
        )
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let err = auth.token().await.expect_err("401 refused");
    let display = format!("{err}");
    let TeregenAuthError::Unauthorized {
        principal,
        key,
        stage,
        base,
        hint,
        ..
    } = err
    else {
        panic!("expected Unauthorized, got {display}");
    };
    for cause in ["unknown", "no key", "does not match", "invalid", "nonce"] {
        assert!(hint.contains(cause), "hint missing {cause:?}: {hint}");
    }
    let expected_fp = ssh_key::PrivateKey::read_openssh_file(fixture("obs", "id_ed25519"))
        .expect("fixture key reads")
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    assert_eq!(principal, PRINCIPAL);
    assert_eq!(
        key, expected_fp,
        "a file-backed key reports the fingerprint that signed, never a path"
    );
    assert_eq!(stage, mtui_datasources::teregen::AuthStage::Verify);
    assert_eq!(base, server.uri());
    assert!(display.contains(PRINCIPAL), "display names principal");
    assert!(display.contains(&expected_fp), "display names the key");
    assert!(
        display.contains("/auth/ssh/verify"),
        "display says which request"
    );
    assert!(display.contains(&server.uri()), "display names the server");
    assert!(
        !display.contains("id_ed25519"),
        "display never prints a key path"
    );
}

/// A `base` carrying userinfo never reaches the `Unauthorized` error: both
/// construction sites store it already sanitized, so matching on the public
/// `base` field and logging it cannot leak credentials — and `Display` needs
/// no redaction of its own. Fails while the error stores the raw base.
#[tokio::test]
async fn userinfo_in_base_never_reaches_unauthorized_error() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "unauthorized"})),
        )
        .mount(&server)
        .await;

    let base = server.uri().replacen("://", "://user:pass@", 1);
    let auth = TeregenAuth::new(
        base,
        PRINCIPAL.to_owned(),
        Some(fixture("obs", "id_ed25519")),
        None,
        HttpClient::new(VerifyPolicy::Default(true)).expect("client builds"),
    )
    .with_store(Some(TokenStore::at(store_file)));
    let err = auth.token().await.expect_err("401 refused");
    let TeregenAuthError::Unauthorized { base, .. } = &err else {
        panic!("expected Unauthorized, got {err:?}");
    };
    assert!(!base.contains("user:pass"), "stored base: {base}");
    assert!(
        base.contains("127.0.0.1"),
        "stored base keeps the host: {base}"
    );
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(!rendered.contains("user:pass"), "leaked: {rendered}");
    }
}

/// The same guarantee through the write-retry path: a `base` carrying
/// userinfo never reaches the `WriteRetry` error either. Fails while that
/// construction site stores the raw base.
#[tokio::test]
async fn userinfo_in_base_never_reaches_the_write_retry_error() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let base = server.uri().replacen("://", "://user:pass@", 1);
    let auth = TeregenAuth::new(
        base,
        PRINCIPAL.to_owned(),
        Some(fixture("obs", "id_ed25519")),
        None,
        HttpClient::new(VerifyPolicy::Default(true)).expect("client builds"),
    )
    .with_store(Some(TokenStore::at(store_file)));
    let url = format!("{}/schema", server.uri());
    let err = auth
        .authenticated_request_with(reqwest::Method::GET, &url, |b| b)
        .await
        .expect_err("a second 401 is surfaced as an error");
    let TeregenAuthError::Unauthorized { base, stage, .. } = &err else {
        panic!("expected Unauthorized, got {err:?}");
    };
    assert_eq!(
        *stage,
        mtui_datasources::teregen::AuthStage::WriteRetry,
        "a double 401 must say which request was refused"
    );
    assert!(!base.contains("user:pass"), "stored base: {base}");
    assert!(
        base.contains("127.0.0.1"),
        "stored base keeps the host: {base}"
    );
    for rendered in [format!("{err}"), format!("{err:?}")] {
        assert!(!rendered.contains("user:pass"), "leaked: {rendered}");
    }
}

/// An agent-backed (fingerprint-configured) key reports the canonical
/// `SHA256:…` fingerprint of the key that signed the verify request.
#[tokio::test]
async fn verify_401_names_the_signing_key_for_agent_backed_keys() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "unauthorized"})),
        )
        .mount(&server)
        .await;

    let key = ssh_key::PrivateKey::read_openssh_file(fixture("obs", "id_ed25519"))
        .expect("fixture key reads");
    let fingerprint = key
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let auth = TeregenAuth::with_agent(
        server.uri(),
        PRINCIPAL.to_owned(),
        None,
        Some(fingerprint.clone()),
        HttpClient::new(VerifyPolicy::Default(true)).expect("client builds"),
        MockAgent::with(vec![key]),
    )
    .with_store(Some(TokenStore::at(store_file)));
    let err = auth.token().await.expect_err("401 refused");
    let TeregenAuthError::Unauthorized {
        principal,
        key,
        stage,
        base,
        ..
    } = err
    else {
        panic!("expected Unauthorized, got {err:?}");
    };
    assert_eq!(principal, PRINCIPAL);
    assert_eq!(key, fingerprint);
    assert_eq!(stage, mtui_datasources::teregen::AuthStage::Verify);
    assert_eq!(base, server.uri());
}

/// 7. verify 200 with a missing `token` → `Protocol`; nothing written to the
///    store.
#[tokio::test]
async fn verify_200_missing_token_is_protocol_and_does_not_cache() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file.clone());
    let err = auth.token().await.expect_err("missing token");
    assert!(matches!(err, TeregenAuthError::Protocol(_)));
    assert!(!store_file.exists(), "nothing should have been cached");
}

/// A `200` body that is not JSON, or lacks the field, names which endpoint
/// answered and what was missing.
#[tokio::test]
async fn a_200_without_the_expected_field_names_the_endpoint() {
    let (_dir, store_file) = store_path();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let err = auth_for(&server, store_file.clone())
        .challenge()
        .await
        .expect_err("not JSON");
    assert!(
        matches!(&err, TeregenAuthError::Protocol(m) if m.starts_with("challenge response is not JSON: ")),
        "{err:?}"
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let err = auth_for(&server, store_file.clone())
        .challenge()
        .await
        .expect_err("no nonce");
    assert!(
        matches!(&err, TeregenAuthError::Protocol(m) if m == "challenge response is missing 'nonce'"),
        "{err:?}"
    );

    let server = MockServer::start().await;
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let err = auth_for(&server, store_file)
        .token()
        .await
        .expect_err("not JSON");
    assert!(
        matches!(&err, TeregenAuthError::Protocol(m) if m.starts_with("verify response is not JSON: ")),
        "{err:?}"
    );
}

/// 10. A second `token()` call is a cache hit: zero additional HTTP.
#[tokio::test]
async fn second_token_call_is_a_cache_hit() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let first = auth.token().await.expect("mints");
    let second = auth.token().await.expect("cache hit");
    assert_eq!(first, second);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "exactly one challenge + one verify, no more: {requests:?}"
    );
}

/// 11. Neither the token, the signature, nor the literal `Authorization`
///     appears in logs for a successful mint or a 401 refusal.
#[tokio::test]
async fn logs_never_contain_token_signature_or_authorization() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    let token = "supersecrettoken".repeat(4);
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": token,
        })))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    let logs = capture_logs(|| async {
        auth.token().await.expect("mints");
    })
    .await;

    assert!(!logs.contains(&token), "logs leaked the token: {logs}");
    assert!(
        !logs.contains("BEGIN SSH SIGNATURE"),
        "logs leaked the signature: {logs}"
    );
    assert!(
        !logs.contains("Authorization"),
        "logs leaked the header: {logs}"
    );
}

/// 11 (continued). The `authenticated_request_with` re-mint dance (case 8) never
/// logs the bearer token either — its own request carries an `Authorization`
/// header at the transport level, which the shared HTTP layer never traces.
#[tokio::test]
async fn logs_never_contain_the_bearer_token_across_a_remint() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    let token = "rematoken".repeat(8);
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": token,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    auth.token().await.expect("initial mint");
    let url = format!("{}/schema", server.uri());
    let logs = capture_logs(|| async {
        auth.authenticated_request_with(reqwest::Method::GET, &url, |b| b)
            .await
            .expect("succeeds after one re-mint");
    })
    .await;

    assert!(
        !logs.contains(&token),
        "logs leaked the bearer token: {logs}"
    );
    assert!(
        !logs.contains("Authorization"),
        "logs leaked the header: {logs}"
    );
}

/// 8. cached token → `401` → re-mint → `200`: success; exactly one
///    challenge+verify pair; two authenticated requests to the protected
///    endpoint.
#[tokio::test]
async fn authenticated_request_remints_once_and_succeeds() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    // Seed the cache so the first attempt is a cache hit, not a fresh mint.
    auth.token().await.expect("initial mint");

    let url = format!("{}/schema", server.uri());
    let response = auth
        .authenticated_request_with(reqwest::Method::GET, &url, |b| b)
        .await
        .expect("succeeds after one re-mint");
    assert_eq!(response.status(), 200);

    let requests = server.received_requests().await.unwrap();
    let challenge_count = requests
        .iter()
        .filter(|r| r.url.path() == "/auth/ssh/challenge")
        .count();
    let verify_count = requests
        .iter()
        .filter(|r| r.url.path() == "/auth/ssh/verify")
        .count();
    let schema_count = requests
        .iter()
        .filter(|r| r.url.path() == "/schema")
        .count();
    assert_eq!(challenge_count, 2, "initial mint + one re-mint");
    assert_eq!(verify_count, 2, "initial mint + one re-mint");
    assert_eq!(schema_count, 2, "one 401 + one retry");
}

/// 9. cached token → `401` → re-mint → `401`: error; **no third**
///    authenticated request.
#[tokio::test]
async fn authenticated_request_does_not_retry_a_second_401() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let auth = auth_for(&server, store_file);
    auth.token().await.expect("initial mint");

    let url = format!("{}/schema", server.uri());
    let err = auth
        .authenticated_request_with(reqwest::Method::GET, &url, |b| b)
        .await
        .expect_err("a second 401 is surfaced as an error");
    assert!(matches!(err, TeregenAuthError::Unauthorized { .. }));

    let requests = server.received_requests().await.unwrap();
    let schema_count = requests
        .iter()
        .filter(|r| r.url.path() == "/schema")
        .count();
    assert_eq!(schema_count, 2, "exactly one retry, no third attempt");
}

/// 12. `format!("{:?}")` of [`TeregenAuth`] never contains token bytes (the
///     struct holds no token field at all — it fetches one per call).
#[tokio::test]
async fn debug_of_teregen_auth_never_contains_a_token() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    let auth = auth_for(&server, store_file);
    let rendered = format!("{auth:?}");
    assert!(rendered.contains("TeregenAuth"));
    assert!(!rendered.to_lowercase().contains("token"));
}

async fn mount_verify_401(server: &MockServer, body: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(body)
        .mount(server)
        .await;
}

fn verify_401(hint: &str) -> ResponseTemplate {
    ResponseTemplate::new(401)
        .set_body_json(serde_json::json!({"error": "unauthorized", "hint": hint}))
}

/// The prose each verify hint maps to, and a phrase only it contains: the
/// generic list of causes must not be what the user reads.
#[tokio::test]
async fn verify_401_names_the_cause_teregen_reports() {
    for (wire, hint, phrase) in [
        (
            "challenge_expired_or_used",
            AuthHint::ChallengeExpiredOrUsed,
            "already used or has expired",
        ),
        (
            "signature_rejected",
            AuthHint::SignatureRejected,
            "does not say which",
        ),
        (
            "key_list_unavailable",
            AuthHint::KeyListUnavailable,
            "refuses every principal",
        ),
    ] {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_challenge_200(&server).await;
        mount_verify_401(&server, verify_401(wire)).await;

        let err = auth_for(&server, store_file)
            .token()
            .await
            .expect_err("401 refused");
        let TeregenAuthError::Unauthorized {
            hint: text,
            server_hint,
            ..
        } = &err
        else {
            panic!("{wire}: expected Unauthorized, got {err:?}");
        };
        assert_eq!(*server_hint, Some(hint), "{wire}");
        assert!(text.contains(phrase), "{wire}: {text}");
        assert!(
            !text.contains("sshkeys.yaml"),
            "{wire}: generic list leaked: {text}"
        );
    }
}

/// No body, a body without a hint and an unknown hint all read as the generic
/// list, and an unknown value is never echoed.
#[tokio::test]
async fn unknown_or_absent_hint_keeps_the_generic_text() {
    for body in [
        ResponseTemplate::new(401),
        ResponseTemplate::new(401).set_body_json(serde_json::json!({"error": "unauthorized"})),
        ResponseTemplate::new(401)
            .set_body_json(serde_json::json!({"error": "unauthorized", "hint": "zzz-marker"})),
        ResponseTemplate::new(401).set_body_string("<html>zzz-marker</html>"),
    ] {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_challenge_200(&server).await;
        mount_verify_401(&server, body).await;

        let err = auth_for(&server, store_file)
            .token()
            .await
            .expect_err("401 refused");
        let TeregenAuthError::Unauthorized {
            hint, server_hint, ..
        } = &err
        else {
            panic!("expected Unauthorized, got {err:?}");
        };
        assert_eq!(*server_hint, None);
        assert!(hint.contains("sshkeys.yaml"), "{hint}");
        for rendered in [err.to_string(), format!("{err:?}")] {
            assert!(!rendered.contains("zzz-marker"), "echoed: {rendered}");
        }
    }
}

/// A write refused after a fresh mint names teregen's cause, not the
/// stale-cache one.
#[tokio::test]
async fn write_retry_401_names_the_cause_teregen_reports() {
    for (wire, hint, phrase) in [
        ("token_missing", AuthHint::TokenMissing, "Authorization"),
        ("token_expired", AuthHint::TokenExpired, "server clock"),
        ("token_unknown", AuthHint::TokenUnknown, "revoked"),
    ] {
        let server = MockServer::start().await;
        let (_dir, store_file) = store_path();
        mount_challenge_200(&server).await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "a".repeat(64),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/schema"))
            .respond_with(verify_401(wire))
            .mount(&server)
            .await;

        let url = format!("{}/schema", server.uri());
        let err = auth_for(&server, store_file)
            .authenticated_request_with(reqwest::Method::GET, &url, |b| b)
            .await
            .expect_err("a second 401 is surfaced");
        let TeregenAuthError::Unauthorized {
            hint: text,
            server_hint,
            stage,
            ..
        } = &err
        else {
            panic!("{wire}: expected Unauthorized, got {err:?}");
        };
        assert_eq!(*stage, AuthStage::WriteRetry);
        assert_eq!(*server_hint, Some(hint), "{wire}");
        assert!(text.contains(phrase), "{wire}: {text}");
        assert!(!text.contains("not a stale cache"), "{wire}: {text}");
    }
}

/// teregen sends `token_missing` only to a client that accepts JSON.
#[tokio::test]
async fn bearer_requests_ask_for_json() {
    use wiremock::matchers::header;

    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/schema"))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let url = format!("{}/schema", server.uri());
    let response = auth_for(&server, store_file)
        .authenticated_request_with(reqwest::Method::GET, &url, |b| b)
        .await
        .expect("request goes through");
    assert_eq!(response.status().as_u16(), 200);
}

async fn challenge_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/auth/ssh/challenge")
        .count()
}

/// An expired or used nonce is retried with a new challenge, once.
#[tokio::test]
async fn an_expired_challenge_is_reminted_once() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(verify_401("challenge_expired_or_used"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "b".repeat(64),
        })))
        .mount(&server)
        .await;

    let token = auth_for(&server, store_file)
        .token()
        .await
        .expect("the second challenge succeeds");
    assert_eq!(token, "b".repeat(64));
    assert_eq!(challenge_count(&server).await, 2);
}

/// A nonce refused as expired twice is surfaced after exactly two challenges:
/// challenges are rate-limited, so this must never loop.
#[tokio::test]
async fn a_second_expired_challenge_is_surfaced_not_retried() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    mount_verify_401(&server, verify_401("challenge_expired_or_used")).await;

    let err = auth_for(&server, store_file)
        .token()
        .await
        .expect_err("still refused");
    assert!(
        matches!(
            err,
            TeregenAuthError::Unauthorized {
                server_hint: Some(AuthHint::ChallengeExpiredOrUsed),
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(challenge_count(&server).await, 2);
}

/// Only an expired challenge earns a second one: a rejected signature would
/// fail the same way again.
#[tokio::test]
async fn a_rejected_signature_is_not_reminted() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_challenge_200(&server).await;
    mount_verify_401(&server, verify_401("signature_rejected")).await;

    auth_for(&server, store_file)
        .token()
        .await
        .expect_err("refused");
    assert_eq!(challenge_count(&server).await, 1);
}
