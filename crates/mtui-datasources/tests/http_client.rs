//! Integration tests for the shared [`HttpClient`] GET-to-bytes path.
//!
//! Ports the behavioral core of upstream `test_support_http.py`'s `get_bytes`
//! group against a real HTTP transport (`wiremock`) instead of a fake session:
//! a 2xx returns the body bytes, and a non-2xx status is surfaced as an error
//! (upstream `response.raise_for_status()`).

use mtui_datasources::{HttpClient, VerifyPolicy};
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
