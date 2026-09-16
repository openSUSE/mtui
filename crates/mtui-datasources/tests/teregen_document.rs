//! Wiremock matrix for the teregen v2 read client
//! (`mtui_datasources::teregen::document`).
//!
//! Covers every [`TeregenV2Error`] status mapping (404/409/503/400), the
//! 200+ETag / 304 conditional-GET pair, a dotted SLFO id round-tripping intact
//! through the path, invalid JSON producing a pointer, a body over the cap,
//! an unreachable base, and the #431 log-secrecy discipline (no URL reaches a
//! `tracing` event).

use mtui_config::Config;
use mtui_datasources::teregen::{DocumentFetch, TeregenV2, TeregenV2Error};
use mtui_datasources::{HttpClient, MAX_API_BODY, VerifyPolicy};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::log_capture::capture_logs;

/// A base URL where nothing listens, so a request fails inside `send` rather
/// than on a status.
const CLOSED_PORT: &str = "http://127.0.0.1:1";

fn client(server: &MockServer) -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, &server.uri())
}

fn unreachable_client() -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, CLOSED_PORT)
}

/// A minimal, schema-valid document body for `id`.
fn minimal_document(id: &str) -> String {
    format!(
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
    )
}

const RRID: &str = "SUSE:Maintenance:1:2";

#[tokio::test]
async fn fresh_200_carries_the_document_and_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(minimal_document(RRID))
                .insert_header("etag", "\"abc123\""),
        )
        .mount(&server)
        .await;

    let fetch = client(&server).fetch_document(RRID, None).await.unwrap();
    let DocumentFetch::Fresh {
        document,
        raw,
        etag,
    } = fetch
    else {
        panic!("expected Fresh, got NotModified");
    };
    assert_eq!(document.id, RRID);
    assert!(raw.contains(RRID));
    assert_eq!(etag.as_deref(), Some("\"abc123\""));
}

/// A dotted SLFO id must reach the server with its colons intact.
#[tokio::test]
async fn dotted_slfo_id_round_trips_through_the_path() {
    let slfo_id = "SUSE:SLFO:1.2:7787";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{slfo_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(slfo_id)))
        .mount(&server)
        .await;

    let fetch = client(&server).fetch_document(slfo_id, None).await.unwrap();
    let DocumentFetch::Fresh { document, .. } = fetch else {
        panic!("expected Fresh");
    };
    assert_eq!(document.id, slfo_id);
}

#[tokio::test]
async fn if_none_match_sent_when_etag_given() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .and(header("if-none-match", "\"abc123\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let fetch = client(&server)
        .fetch_document(RRID, Some("\"abc123\""))
        .await
        .unwrap();
    assert_eq!(fetch, DocumentFetch::NotModified);
}

#[tokio::test]
async fn no_etag_omits_if_none_match() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID)))
        .mount(&server)
        .await;

    let _ = client(&server).fetch_document(RRID, None).await;
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("if-none-match").is_none());
}

/// Reads are anonymous: no `Authorization` header on any request.
#[tokio::test]
async fn sends_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID)))
        .mount(&server)
        .await;

    let _ = client(&server).fetch_document(RRID, None).await;
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn not_found_maps_to_no_document_yet() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::NotFound));
    assert!(err.to_string().contains("regenerate"));
}

#[tokio::test]
async fn conflict_maps_to_stale() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(serde_json::json!({"error": "stale document"})),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::Stale));
    assert!(err.to_string().contains("stale"));
}

#[tokio::test]
async fn service_unavailable_maps_to_generating() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::Generating));
}

/// A `400` carries the server's own detail message.
#[tokio::test]
async fn bad_request_carries_the_servers_detail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "errors": [{"message": "String does not match ^[A-Za-z0-9:.]+$.", "path": "/id"}]
        })))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    let TeregenV2Error::RejectedId { detail } = err else {
        panic!("expected RejectedId, got {err:?}");
    };
    // Not the `{"error": ...}` shape, so the raw body is the fallback detail.
    assert!(detail.contains("does not match"));
}

#[tokio::test]
async fn invalid_json_reports_a_pointer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id": "x"}"#))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    let TeregenV2Error::Invalid(doc_err) = err else {
        panic!("expected Invalid, got {err:?}");
    };
    assert!(!doc_err.pointer.is_empty());
}

#[tokio::test]
async fn body_over_the_cap_is_rejected() {
    let server = MockServer::start().await;
    let oversized = "x".repeat(MAX_API_BODY + 1);
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::BodyTooLarge));
}

#[tokio::test]
async fn unreachable_base_is_a_transport_error() {
    let err = unreachable_client()
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::Transport(_)));
}

/// #431: neither the transport-failure log line nor the `Transport` message
/// itself may carry a reqwest-appended URL.
#[tokio::test]
async fn transport_failure_logs_and_errors_without_a_url() {
    let mut err = None;
    let logs = capture_logs(|| async {
        err = unreachable_client().fetch_document(RRID, None).await.err();
    })
    .await;

    let Some(TeregenV2Error::Transport(msg)) = &err else {
        panic!("expected Transport, got {err:?}");
    };
    assert!(!msg.contains(" for url ("), "error rendered the URL: {msg}");
    let line = logs
        .lines()
        .find(|l| l.contains("TeReGen v2 GET"))
        .unwrap_or_else(|| panic!("no `TeReGen v2 GET` line in capture: {logs}"));
    assert!(!line.contains(" for url ("), "log rendered the URL: {line}");
}

/// `TeregenV2::new` (the `Config`-driven constructor, mirroring
/// `TeReGen::new`) builds a working client, not just `with_client`.
#[tokio::test]
async fn new_builds_a_working_client_from_config() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(minimal_document(RRID)))
        .mount(&server)
        .await;

    let client = TeregenV2::new(&Config::default(), &server.uri()).unwrap();
    let fetch = client.fetch_document(RRID, None).await.unwrap();
    assert!(matches!(fetch, DocumentFetch::Fresh { .. }));
}

/// An unmodelled non-2xx status (not one of 304/404/409/503/400) falls
/// through to the generic `error_for_status` arm.
#[tokio::test]
async fn an_unmodelled_status_is_a_transport_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    assert!(matches!(err, TeregenV2Error::Transport(_)));
}

/// A `400` body's `error` detail longer than the cap is truncated.
#[tokio::test]
async fn bad_request_detail_is_truncated_at_the_cap() {
    let server = MockServer::start().await;
    let long_detail = "x".repeat(3000);
    Mock::given(method("GET"))
        .and(path(format!("/reports/{RRID}")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": long_detail})),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .fetch_document(RRID, None)
        .await
        .unwrap_err();
    let TeregenV2Error::RejectedId { detail } = err else {
        panic!("expected RejectedId, got {err:?}");
    };
    assert_eq!(detail.len(), 2048);
}
