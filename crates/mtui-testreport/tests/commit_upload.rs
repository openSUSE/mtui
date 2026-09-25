//! Wiremock coverage for `mtui_testreport::commit_upload::upload_current`:
//! the document-then-artifacts sequence, the `If-Match` precondition, the
//! adopt-only-on-success rule, and per-artifact failure reporting.

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_datasources::teregen::{ArtifactUploadError, TeregenAuth, TeregenV2, TokenStore};
use mtui_datasources::{HttpClient, VerifyPolicy};
use mtui_testreport::{CommitUploadError, TestReportBase, upload_current};
use mtui_types::report_document::ReportDocument;
use std::str::FromStr;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RRID: &str = "SUSE:Maintenance:1:2";
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PRINCIPAL: &str = "alice";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/obs")
        .join(name)
}

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

fn auth_for(server: &MockServer, store_path: PathBuf) -> TeregenAuth {
    TeregenAuth::new(
        server.uri(),
        PRINCIPAL.to_owned(),
        Some(fixture("id_ed25519")),
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

/// A `TestReportBase` with `document`/`document_etag` set as if freshly
/// loaded from teregen v2.
fn base_with_document(etag: &str) -> TestReportBase {
    let mut base = TestReportBase::new(Config::default());
    base.document = Some(minimal_document(RRID));
    base.document_etag = Some(etag.to_owned());
    base
}

// --- all-success ---

#[tokio::test]
async fn happy_path_sends_if_match_and_adopts_the_new_document_and_etag() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    mount_auth_success(&server).await;

    let mut response_doc = minimal_document(RRID);
    response_doc.comment = mtui_types::report_document::Req(Some("server re-read".to_owned()));
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}")))
        .and(header("if-match", "\"stale-etag\""))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_string(serde_json::to_string(&response_doc).unwrap())
                .insert_header("etag", "\"fresh-etag\""),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/h1.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let mut base = base_with_document("\"stale-etag\"");
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("h1.log");
    std::fs::write(&log, b"log body").unwrap();

    let report = upload_current(
        &mut base,
        &client(&server, store_file),
        vec![("h1.log".to_owned(), log)],
    )
    .await
    .expect("all-success");

    assert_eq!(report.etag.as_deref(), Some("\"fresh-etag\""));
    assert_eq!(report.artifacts.len(), 1);
    assert!(report.artifacts[0].1.is_ok());
    assert_eq!(base.document_etag.as_deref(), Some("\"fresh-etag\""));
    assert_eq!(
        base.document.as_ref().unwrap().comment.0,
        Some("server re-read".to_owned())
    );
}

/// The document PUT must precede any artifact PUT.
#[tokio::test]
async fn document_is_sent_before_artifacts() {
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
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/a.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/b.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let mut base = base_with_document("\"x\"");
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.log");
    let b = tmp.path().join("b.log");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();

    upload_current(
        &mut base,
        &client(&server, store_file),
        vec![("a.log".to_owned(), a), ("b.log".to_owned(), b)],
    )
    .await
    .expect("all-success");

    let requests = server.received_requests().await.unwrap();
    let put_paths: Vec<&str> = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::PUT)
        .map(|r| r.url.path())
        .collect();
    assert_eq!(
        put_paths,
        vec![
            format!("/reports/{RRID}"),
            format!("/reports/{RRID}/artifacts/a.log"),
            format!("/reports/{RRID}/artifacts/b.log"),
        ]
    );
}

// --- 412: zero artifact PUTs, local state unchanged ---

#[tokio::test]
async fn precondition_failed_sends_no_artifacts_and_leaves_state_unchanged() {
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

    let mut base = base_with_document("\"stale\"");
    let before_doc = base.document.clone();
    let before_etag = base.document_etag.clone();
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("h1.log");
    std::fs::write(&log, b"x").unwrap();

    let err = upload_current(
        &mut base,
        &client(&server, store_file),
        vec![("h1.log".to_owned(), log)],
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CommitUploadError::Document(_)));

    assert_eq!(base.document, before_doc, "document must be unchanged");
    assert_eq!(base.document_etag, before_etag, "etag must be unchanged");

    let requests = server.received_requests().await.unwrap();
    let artifact_puts = requests
        .iter()
        .filter(|r| r.url.path().contains("/artifacts/"))
        .count();
    assert_eq!(artifact_puts, 0, "a 412 must send no artifacts");
}

// --- partial artifact failure ---

#[tokio::test]
async fn one_artifact_413_does_not_block_the_others() {
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
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/ok1.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/big.log")))
        .respond_with(ResponseTemplate::new(413))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/reports/{RRID}/artifacts/ok2.log")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let mut base = base_with_document("\"x\"");
    let tmp = tempfile::tempdir().unwrap();
    let mk = |name: &str| {
        let p = tmp.path().join(name);
        std::fs::write(&p, b"x").unwrap();
        (name.to_owned(), p)
    };

    let report = upload_current(
        &mut base,
        &client(&server, store_file),
        vec![mk("ok1.log"), mk("big.log"), mk("ok2.log")],
    )
    .await
    .expect("document succeeded; per-artifact results carry the failure");

    assert_eq!(report.artifacts.len(), 3);
    assert!(report.artifacts[0].1.is_ok(), "ok1.log should succeed");
    assert!(
        matches!(&report.artifacts[1].1, Err(ArtifactUploadError::TooLarge { name }) if name == "big.log"),
        "{:?}",
        report.artifacts[1]
    );
    assert!(report.artifacts[2].1.is_ok(), "ok2.log should still upload");
}

// --- no etag: zero requests ---

#[tokio::test]
async fn no_etag_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    // Deliberately no mocks mounted: any request is a hard failure.
    let mut base = TestReportBase::new(Config::default());
    base.document = Some(minimal_document(RRID));
    // document_etag stays None.

    let err = upload_current(&mut base, &client(&server, store_file), vec![])
        .await
        .unwrap_err();
    assert!(matches!(err, CommitUploadError::NoEtag));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "no etag must send nothing: {requests:?}"
    );
}

#[tokio::test]
async fn no_document_sends_zero_requests() {
    let server = MockServer::start().await;
    let (_dir, store_file) = store_path();
    let mut base = TestReportBase::new(Config::default());

    let err = upload_current(&mut base, &client(&server, store_file), vec![])
        .await
        .unwrap_err();
    assert!(matches!(err, CommitUploadError::NoEtag));

    let requests = server.received_requests().await.unwrap();
    assert!(requests.is_empty());
}
