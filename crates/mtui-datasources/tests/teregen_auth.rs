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

use mtui_datasources::teregen::{TeregenAuth, TeregenAuthError, TokenStore};
use mtui_datasources::{HttpClient, VerifyPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::log_capture::capture_logs;

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

/// 6. verify 401 → `Unauthorized` whose hint names the indistinguishable
///    causes.
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
    let TeregenAuthError::Unauthorized { hint } = err else {
        panic!("expected Unauthorized, got {err:?}");
    };
    for cause in ["unknown", "no key", "does not match", "invalid", "nonce"] {
        assert!(hint.contains(cause), "hint missing {cause:?}: {hint}");
    }
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

/// 11 (continued). The `authenticated_request` re-mint dance (case 8) never
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
        auth.authenticated_request(reqwest::Method::GET, &url)
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
        .authenticated_request(reqwest::Method::GET, &url)
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
        .authenticated_request(reqwest::Method::GET, &url)
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
