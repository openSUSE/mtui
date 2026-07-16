//! Integration tests for the shared [`HttpClient`] GET-to-bytes path.
//!
//! Ports the behavioral core of upstream `test_support_http.py`'s `get_bytes`
//! group against a real HTTP transport (`wiremock`) instead of a fake session:
//! a 2xx returns the body bytes, and a non-2xx status is surfaced as an error
//! (upstream `response.raise_for_status()`).

use mtui_datasources::{HttpClient, HttpError, VerifyPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a plain (verifying-default) client for the wiremock HTTP endpoint.
/// wiremock serves plain HTTP, so the TLS posture is never exercised here.
fn client() -> HttpClient {
    HttpClient::new(VerifyPolicy::Default(true)).expect("client builds")
}

#[tokio::test]
async fn get_bytes_returns_response_content() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/file"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload-bytes".to_vec()))
        .mount(&server)
        .await;

    let out = client()
        .get_bytes(&format!("{}/file", server.uri()))
        .await
        .expect("2xx returns bytes");

    assert_eq!(out, b"payload-bytes");
}

#[tokio::test]
async fn get_bytes_raises_on_http_error_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client()
        .get_bytes(&format!("{}/missing", server.uri()))
        .await
        .expect_err("404 is an error");

    // reqwest surfaces the non-2xx as a status error wrapped in HttpError.
    assert!(
        matches!(err, mtui_datasources::HttpError::Request(_)),
        "expected HttpError::Request, got {err:?}"
    );
}

#[tokio::test]
async fn get_bytes_returns_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/empty"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let out = client()
        .get_bytes(&format!("{}/empty", server.uri()))
        .await
        .expect("empty 2xx body is Ok");

    assert!(out.is_empty());
}

// --- Bounded response bodies (th4o.9) ---

#[tokio::test]
async fn get_bytes_capped_accepts_body_at_the_limit() {
    let server = MockServer::start().await;
    let body = vec![b'x'; 64];
    Mock::given(method("GET"))
        .and(path("/exact"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let out = client()
        .get_bytes_capped(&format!("{}/exact", server.uri()), 64)
        .await
        .expect("a body exactly at the limit is accepted");

    assert_eq!(out, body);
}

#[tokio::test]
async fn get_bytes_capped_rejects_oversized_content_length_early() {
    let server = MockServer::start().await;
    // An honest Content-Length (wiremock sets it from the body) larger than the
    // cap must be rejected before the body is read: seen = Some(len).
    let body = vec![b'x'; 1024];
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let err = client()
        .get_bytes_capped(&format!("{}/big", server.uri()), 100)
        .await
        .expect_err("oversized body is rejected");

    match err {
        HttpError::BodyTooLarge { limit, seen } => {
            assert_eq!(limit, 100);
            assert_eq!(seen, Some(1024), "honest Content-Length rejected early");
        }
        other => panic!("expected BodyTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn get_bytes_capped_rejects_chunked_over_limit_mid_stream() {
    let server = MockServer::start().await;
    // Transfer-Encoding: chunked with no Content-Length: reqwest reports an
    // unknown length, so the cap must trip while streaming (seen = None).
    let body = vec![b'x'; 1024];
    Mock::given(method("GET"))
        .and(path("/chunked"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("transfer-encoding", "chunked")
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;

    let err = client()
        .get_bytes_capped(&format!("{}/chunked", server.uri()), 100)
        .await
        .expect_err("oversized chunked body is rejected");

    match err {
        HttpError::BodyTooLarge { limit, seen } => {
            assert_eq!(limit, 100);
            assert_eq!(seen, None, "unknown-length body rejected mid-stream");
        }
        other => panic!("expected BodyTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn get_bytes_capped_returns_normal_json_body() {
    let server = MockServer::start().await;
    let json = br#"{"jobs":[]}"#.to_vec();
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(json.clone()))
        .mount(&server)
        .await;

    let out = client()
        .get_bytes_capped(&format!("{}/api", server.uri()), 16 * 1024)
        .await
        .expect("a small JSON body under the cap succeeds");

    assert_eq!(out, json);
}

#[tokio::test]
async fn body_too_large_error_message_carries_no_url() {
    let err = HttpError::BodyTooLarge {
        limit: 42,
        seen: Some(1000),
    };
    let msg = err.to_string();
    assert!(msg.contains("42"), "message names the limit: {msg}");
    assert!(!msg.contains("http"), "message must not leak a URL: {msg}");
}
