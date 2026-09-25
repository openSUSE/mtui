//! Wiremock matrix for the teregen v2 artifact-upload client
//! (`mtui_datasources::teregen::document::TeregenV2::upload_artifact`).
//!
//! Covers the `201`/`200` created-vs-replaced distinction (body bytes and
//! content-type asserted), every [`ArtifactUploadError`] status mapping, the
//! pre-flight zero-request guarantees (bad name, oversize body), and the
//! `401` re-mint dance shared with the document write client.

use std::path::{Path, PathBuf};

use mtui_datasources::teregen::{
    ArtifactStored, ArtifactUploadError, TeregenAuth, TeregenV2, TokenStore,
};
use mtui_datasources::{HttpClient, MAX_API_BODY, VerifyPolicy};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RRID: &str = "SUSE:Maintenance:1:2";
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PRINCIPAL: &str = "alice";

fn fixture(dir: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

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

fn client(server: &MockServer, store_file: PathBuf) -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, &server.uri()).with_auth(auth_for(server, store_file))
}

async fn mount_auth_success(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/ssh/challenge"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": NONCE})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/auth/ssh/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "a".repeat(64),
        })))
        .mount(server)
        .await;
}

// --- 201/200 happy path ---

#[tokio::test]
async fn created_on_201_with_body_and_content_type_asserted() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .and(header("content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let outcome = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"log body".to_vec())
        .await
        .expect("201 succeeds");
    assert_eq!(outcome, ArtifactStored::Created);

    let requests = server.received_requests().await.unwrap();
    let put_req = requests
        .iter()
        .find(|r| r.url.path() == format!("/reports/{RRID}/artifacts/h1.log"))
        .unwrap();
    assert_eq!(put_req.body, b"log body");
}

#[tokio::test]
async fn replaced_on_200() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let outcome = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"log body".to_vec())
        .await
        .expect("200 succeeds");
    assert_eq!(outcome, ArtifactStored::Replaced);
}

// --- 400 / 413 / 422 ---

#[tokio::test]
async fn bad_request_invalid_id_is_rejected_id() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid id"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    let ArtifactUploadError::RejectedId { detail } = err else {
        panic!("expected RejectedId, got {err:?}");
    };
    assert_eq!(detail, "invalid id");
}

#[tokio::test]
async fn server_413_maps_to_too_large() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(ResponseTemplate::new(413))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::TooLarge { name } if name == "h1.log"));
}

#[tokio::test]
async fn server_422_maps_to_invalid_name() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(
            ResponseTemplate::new(422)
                .set_body_json(serde_json::json!({"error": "invalid artifact name"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::InvalidName { name } if name == "h1.log"));
}

// --- 503: busy vs generating ---

#[tokio::test]
async fn service_unavailable_busy_is_distinguished_from_generating() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "busy"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::Busy));
}

#[tokio::test]
async fn service_unavailable_generating_body() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "generating"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::Generating));
}

// --- pre-flight: zero requests ---

#[tokio::test]
async fn preflight_bad_name_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    // Deliberately no mocks mounted: any request is a hard failure.
    let err = client(&server, store_file)
        .upload_artifact(RRID, "../etc/passwd", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::InvalidName { name } if name == "../etc/passwd"));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "pre-flight must send nothing: {requests:?}"
    );
}

#[tokio::test]
async fn preflight_dotfile_name_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    let err = client(&server, store_file)
        .upload_artifact(RRID, ".hidden", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::InvalidName { name } if name == ".hidden"));

    let requests = server.received_requests().await.unwrap();
    assert!(requests.is_empty());
}

#[tokio::test]
async fn preflight_oversize_body_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    let oversize = vec![0u8; MAX_API_BODY + 1];

    let err = client(&server, store_file)
        .upload_artifact(RRID, "h1.log", oversize)
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::TooLarge { name } if name == "h1.log"));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "pre-flight must send nothing: {requests:?}"
    );
}

#[tokio::test]
async fn not_configured_without_auth_sends_zero_requests() {
    let server = MockServer::start().await;
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    let c = TeregenV2::with_client(http, &server.uri());

    let err = c
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactUploadError::NotConfigured));

    let requests = server.received_requests().await.unwrap();
    assert!(requests.is_empty());
}

// --- 401 re-mint dance ---

#[tokio::test]
async fn authorized_after_one_remint_writes_exactly_once() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let c = client(&server, store_file);
    let outcome = c
        .upload_artifact(RRID, "h1.log", b"x".to_vec())
        .await
        .expect("succeeds after one re-mint");
    assert_eq!(outcome, ArtifactStored::Created);

    let requests = server.received_requests().await.unwrap();
    let put_count = requests
        .iter()
        .filter(|r| r.url.path() == format!("/reports/{RRID}/artifacts/h1.log"))
        .count();
    assert_eq!(
        put_count, 2,
        "one 401 + one retry, and the write lands once"
    );
}
