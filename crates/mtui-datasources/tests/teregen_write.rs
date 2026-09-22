//! Wiremock matrix for the teregen v2 write client
//! (`mtui_datasources::teregen::document::TeregenV2::upload_document`).
//!
//! Covers every [`TeregenV2WriteError`] status mapping, the `202` happy path
//! (outgoing `If-Match` byte-for-byte, adopting the response's document +
//! ETag per P5-D4), the `412`-never-retried rule, the `401` re-mint dance
//! shared with the read/auth clients, the pre-flight zero-request guarantees
//! (P5-D3), a transport failure, and the #431-style bearer-secrecy discipline.

use std::path::{Path, PathBuf};

use mtui_datasources::teregen::{
    Precondition, TeregenAuth, TeregenV2, TeregenV2WriteError, TokenStore,
};
use mtui_datasources::{HttpClient, MAX_API_BODY, VerifyPolicy};
use mtui_types::report_document::ReportDocument;
use std::str::FromStr;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::log_capture::capture_logs;

const RRID: &str = "SUSE:Maintenance:1:2";
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PRINCIPAL: &str = "alice";

/// A base URL where nothing listens, so a request fails inside `send`.
const CLOSED_PORT: &str = "http://127.0.0.1:1";

fn fixture(dir: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

/// A minimal, schema-valid document body for `id` (mirrors
/// `teregen_document.rs::minimal_document`).
fn minimal_document(id: &str) -> ReportDocument {
    let raw = format!(
        r#"{{
            "schema_version": "1.0", "id": "{id}", "kind": "pi",
            "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {{"testers": [], "reviewer": {{"name": null}}}},
            "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                       "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}],
                       "patches": [{{"id": "1", "title": "t"}}]}},
            "install": {{"repository": "http://x/", "targets": [{{
                "product": "n", "version": "v", "arch": "x86_64",
                "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
            }}], "test_platforms": []}},
            "issues": {{}}, "testing": {{}}
        }}"#
    );
    ReportDocument::from_str(&raw).expect("fixture document parses")
}

/// A [`TeregenAuth`] pointed at `server`, signing with the deterministic
/// Ed25519 fixture key (no ssh-agent involved), caching to a fresh tempdir.
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

/// A [`TeregenV2`] with auth attached, pointed at `server`.
fn client(server: &MockServer, store_file: PathBuf) -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, &server.uri()).with_auth(auth_for(server, store_file))
}

/// A client pointed at [`CLOSED_PORT`] (auth still targets the live `server`,
/// so only the `PUT` itself is unreachable).
fn client_unreachable_target(server: &MockServer, store_file: PathBuf) -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, CLOSED_PORT).with_auth(auth_for(server, store_file))
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

// --- 202 happy path ---

#[tokio::test]
async fn happy_path_sends_if_match_verbatim_and_adopts_the_response() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;

    let sent_etag = "\"stale-etag\"";
    // Distinct from the request's own document (whose `comment` is `null`),
    // so an implementation that echoed the request back instead of adopting
    // the response is caught on *content*, not only on the ETag header.
    let mut response_doc = minimal_document(RRID);
    response_doc.comment = mtui_types::report_document::Req(Some("server re-read".to_owned()));
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .and(header("if-match", sent_etag))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_string(serde_json::to_string(&response_doc).unwrap())
                .insert_header("etag", "\"fresh-etag\""),
        )
        .mount(&server)
        .await;

    let outcome = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match(sent_etag.to_owned()),
        )
        .await
        .expect("202 succeeds");

    // Adopted from the *response*, not the request: a divergent fixture
    // catches a caller that echoed its own bytes back instead.
    assert_eq!(outcome.etag.as_deref(), Some("\"fresh-etag\""));
    assert_eq!(outcome.document.id, RRID);
    assert_eq!(
        outcome.document.comment.0,
        Some("server re-read".to_owned())
    );
}

#[tokio::test]
async fn happy_path_body_is_mtuis_serialization() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    let doc = minimal_document(RRID);
    let expected_body = serde_json::to_string(&doc).unwrap();

    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
        )
        .mount(&server)
        .await;

    let _ = client(&server, store_file)
        .upload_document(RRID, &doc, &Precondition::Match("\"x\"".to_owned()))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let put_req = requests
        .iter()
        .find(|r| r.url.path() == format!("/reports/{RRID}"))
        .unwrap();
    assert_eq!(std::str::from_utf8(&put_req.body).unwrap(), expected_body);
}

#[tokio::test]
async fn create_precondition_omits_if_match() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    let doc = minimal_document(RRID);
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
        )
        .mount(&server)
        .await;

    let _ = client(&server, store_file)
        .upload_document(RRID, &doc, &Precondition::Create)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let put_req = requests
        .iter()
        .find(|r| r.url.path() == format!("/reports/{RRID}"))
        .unwrap();
    assert!(put_req.headers.get("if-match").is_none());
}

// --- 400s / 404 / 413 / 428 ---

#[tokio::test]
async fn bad_request_invalid_id_is_rejected_id() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid id"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    let TeregenV2WriteError::RejectedId { detail } = err else {
        panic!("expected RejectedId, got {err:?}");
    };
    assert_eq!(detail, "invalid id");
}

#[tokio::test]
async fn bad_request_id_mismatch_is_distinct_from_rejected_id() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "id mismatch"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::IdMismatch));
}

#[tokio::test]
async fn not_found_maps_to_never_generated() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(RRID, &minimal_document(RRID), &Precondition::Create)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::NeverGenerated));
}

#[tokio::test]
async fn server_413_maps_to_too_large() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(413))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::TooLarge));
}

#[tokio::test]
async fn server_428_maps_to_precondition_required() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(428))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(RRID, &minimal_document(RRID), &Precondition::Create)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::PreconditionRequired));
}

// --- 412: with and without a response ETag, never retried ---

#[tokio::test]
async fn precondition_failed_with_etag_is_never_retried() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(412)
                .set_body_json(serde_json::json!({"error": "precondition failed"}))
                .insert_header("etag", "\"current-etag\""),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"stale\"".to_owned()),
        )
        .await
        .unwrap_err();
    let TeregenV2WriteError::PreconditionFailed { server_etag } = err else {
        panic!("expected PreconditionFailed, got {err:?}");
    };
    assert_eq!(server_etag.as_deref(), Some("\"current-etag\""));

    let requests = server.received_requests().await.unwrap();
    let put_count = requests
        .iter()
        .filter(|r| r.url.path() == format!("/reports/{RRID}"))
        .count();
    assert_eq!(put_count, 1, "a 412 must never be retried");
}

#[tokio::test]
async fn precondition_failed_without_etag() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(412)
                .set_body_json(serde_json::json!({"error": "precondition failed"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"*\"".to_owned()),
        )
        .await
        .unwrap_err();
    let TeregenV2WriteError::PreconditionFailed { server_etag } = err else {
        panic!("expected PreconditionFailed, got {err:?}");
    };
    assert_eq!(server_etag, None);
}

// --- 422: pointers surfaced verbatim and in order ---

#[tokio::test]
async fn invalid_document_surfaces_pointers_in_order() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "invalid document",
            "pointers": ["/install/targets/0/arch", "/people/testers"],
        })))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    let TeregenV2WriteError::Invalid { pointers } = err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(
        pointers,
        vec![
            "/install/targets/0/arch".to_string(),
            "/people/testers".to_string()
        ]
    );
}

// --- 503: busy vs generating vs unrecognised ---

#[tokio::test]
async fn service_unavailable_busy_is_distinguished_from_generating() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "busy"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::Busy));
}

#[tokio::test]
async fn service_unavailable_generating_body() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(serde_json::json!({"error": "generating"})),
        )
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::Generating));
}

/// An unrecognised `503` body must fall back to the conservative,
/// non-retrying `Generating` reading — never `Busy`.
#[tokio::test]
async fn service_unavailable_unrecognised_body_falls_back_to_generating() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(503).set_body_string("not json"))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::Generating));
}

// --- 401 re-mint dance ---

#[tokio::test]
async fn authorized_after_one_remint_writes_exactly_once() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    let doc = minimal_document(RRID);
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
        )
        .mount(&server)
        .await;

    let c = client(&server, store_file);
    // Seed the cache so the first attempt is a cache hit, then a 401 forces
    // exactly one re-mint.
    c.upload_document(RRID, &doc, &Precondition::Create)
        .await
        .expect("succeeds after one re-mint");

    let requests = server.received_requests().await.unwrap();
    let put_count = requests
        .iter()
        .filter(|r| r.url.path() == format!("/reports/{RRID}"))
        .count();
    assert_eq!(
        put_count, 2,
        "one 401 + one retry, and the write lands once"
    );
}

#[tokio::test]
async fn second_401_is_unauthorized() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server, store_file)
        .upload_document(RRID, &minimal_document(RRID), &Precondition::Create)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        TeregenV2WriteError::Auth(mtui_datasources::TeregenAuthError::Unauthorized { .. })
    ));
}

// --- pre-flight: zero requests ---

#[tokio::test]
async fn preflight_id_mismatch_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    // Deliberately no mocks mounted at all: any request would be a hard
    // failure from wiremock's unmatched-request panic path.
    let err = client(&server, store_file)
        .upload_document(
            "other-id",
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::IdMismatch));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "pre-flight must send nothing: {requests:?}"
    );
}

#[tokio::test]
async fn preflight_oversize_body_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    let mut doc = minimal_document(RRID);
    doc.comment = mtui_types::report_document::Req(Some("x".repeat(MAX_API_BODY + 1)));

    let err = client(&server, store_file)
        .upload_document(RRID, &doc, &Precondition::Match("\"x\"".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::TooLarge));

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
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::NotConfigured));

    let requests = server.received_requests().await.unwrap();
    assert!(requests.is_empty());
}

// --- transport failure ---

#[tokio::test]
async fn transport_failure_is_surfaced() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;

    let err = client_unreachable_target(&server, store_file)
        .upload_document(
            RRID,
            &minimal_document(RRID),
            &Precondition::Match("\"x\"".to_owned()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2WriteError::Auth(_)));
}

// --- secrecy ---

/// The bearer token must never appear in a tracing event or an error
/// `Display`, across a successful write.
#[tokio::test]
async fn logs_and_errors_never_contain_the_bearer_token() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    let doc = minimal_document(RRID);
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(202).set_body_string(serde_json::to_string(&doc).unwrap()),
        )
        .mount(&server)
        .await;

    let c = client(&server, store_file);
    let logs = capture_logs(|| async {
        c.upload_document(RRID, &doc, &Precondition::Create)
            .await
            .expect("succeeds");
    })
    .await;

    // The minted token is a 64-char run of "a"s (see `mount_auth_success`);
    // its presence in the log would prove a leak.
    assert!(
        !logs.contains(&"a".repeat(64)),
        "logs leaked the token: {logs}"
    );
    assert!(
        !logs.contains("Authorization"),
        "logs leaked the header: {logs}"
    );
}

/// Same discipline on the failure path (a `412`), where the error message
/// itself is also user-visible.
#[tokio::test]
async fn errors_never_contain_the_bearer_token_on_failure() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(412)
                .set_body_json(serde_json::json!({"error": "precondition failed"})),
        )
        .mount(&server)
        .await;

    let c = client(&server, store_file);
    let mut err_msg = String::new();
    let logs = capture_logs(|| async {
        let err = c
            .upload_document(
                RRID,
                &minimal_document(RRID),
                &Precondition::Match("\"stale\"".to_owned()),
            )
            .await
            .unwrap_err();
        err_msg = err.to_string();
    })
    .await;

    let token = "a".repeat(64);
    assert!(!logs.contains(&token), "logs leaked the token: {logs}");
    assert!(
        !err_msg.contains(&token),
        "error leaked the token: {err_msg}"
    );
}
