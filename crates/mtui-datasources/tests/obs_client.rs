//! Integration tests for the native OBS HTTP transport against a real HTTP
//! transport (`wiremock`).
//!
//! Covers GET/POST
//! success (headers + XML body), the `<status><summary>` error envelope → typed
//! [`ObsError::Api`], the empty-summary fallback for a non-XML body, and the
//! coarse between-calls budget abort. Auth is [`NoAuth`] — the SSH-signature
//! signer lands in a later subtask (G1c).
//!
//! A mock server can't forge a TLS handshake failure, so there is no test that
//! forges a TLS error directly. The transport-error branch is instead
//! covered by pointing the client at an unroutable/closed endpoint so `request`
//! returns [`ObsError::Http`]; the `is_ssl_verification_error` mapping itself is
//! unit-tested in `src/http.rs`.

use std::sync::Arc;
use std::time::Duration;

use super::log_capture::capture_logs;
use mtui_datasources::VerifyPolicy;
use mtui_datasources::obs::{NoAuth, ObsClient, ObsError};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a client whose API base is `server`, with a generous budget + NoAuth.
fn client_for(server: &MockServer) -> ObsClient {
    ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds")
}

#[tokio::test]
async fn get_success_sets_accept_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/1"))
        .and(header("Accept", "application/xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<request/>"))
        .mount(&server)
        .await;

    let body = client_for(&server)
        .get("request/1", &[("withfullhistory", "1".to_owned())])
        .await
        .expect("get succeeds");
    assert_eq!(body, "<request/>");
}

#[tokio::test]
async fn post_sends_xml_body_and_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/comments/request/1"))
        .and(header("Accept", "application/xml"))
        .and(header("Content-Type", "application/xml; charset=utf-8"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&server)
        .await;

    client_for(&server)
        .post("comments/request/1", &[], "a comment")
        .await
        .expect("post succeeds");

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body, b"a comment");
}

#[tokio::test]
async fn non_2xx_raises_api_error_with_summary() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/9"))
        .respond_with(ResponseTemplate::new(404).set_body_string(
            r#"<status code="not_found"><summary>Request 9 not found</summary></status>"#,
        ))
        .mount(&server)
        .await;

    let err = client_for(&server)
        .get("request/9", &[])
        .await
        .expect_err("404 is an error");
    match err {
        ObsError::Api {
            status, summary, ..
        } => {
            assert_eq!(status, 404);
            assert_eq!(summary, "Request 9 not found");
        }
        other => panic!("expected ObsError::Api, got {other:?}"),
    }
}

#[tokio::test]
async fn non_2xx_non_xml_body_has_empty_summary() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Error"))
        .mount(&server)
        .await;

    let err = client_for(&server)
        .get("x", &[])
        .await
        .expect_err("500 is an error");
    match err {
        ObsError::Api {
            status, summary, ..
        } => {
            assert_eq!(status, 500);
            assert_eq!(summary, "");
        }
        other => panic!("expected ObsError::Api, got {other:?}"),
    }
}

#[tokio::test]
async fn between_calls_budget_aborts_next_call() {
    // A zero budget means the deadline is already in the past by the time the
    // first call checks it.
    let server = MockServer::start().await;
    let client = ObsClient::new(
        &server.uri(),
        Duration::from_secs(0),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds");

    let err = client
        .get("request/1", &[])
        .await
        .expect_err("exhausted budget aborts");
    assert!(matches!(err, ObsError::Timeout(_)), "got {err:?}");
}

#[tokio::test]
async fn transport_error_maps_to_http_variant() {
    // Point at the discard/unreachable port on the loopback (port 9, RFC 863),
    // where nothing listens, so the request fails at the transport layer and
    // exercises the ObsError::Http path. (A dropped MockServer's port can be
    // reused by another test, so we use a fixed non-listening address instead.)
    let client = ObsClient::new(
        "http://127.0.0.1:9",
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds");

    let err = client
        .get("request/1", &[])
        .await
        .expect_err("connection refused is an error");
    assert!(matches!(err, ObsError::Http(_)), "got {err:?}");
}

#[tokio::test]
async fn logs_and_api_error_redact_url_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/9"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string(r#"<status code="not_found"><summary>nope</summary></status>"#),
        )
        .mount(&server)
        .await;

    // Embed credentials in the API base authority.
    let base = server.uri().replace("http://", "http://user:s3cret@");
    let client = ObsClient::new(
        &base,
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds");

    let mut err = String::new();
    let logs = capture_logs(|| async {
        let e = client.get("request/9", &[]).await.expect_err("404");
        err = format!("{e:?}");
    })
    .await;

    // The debug request line and the warn/API-error url are all redacted.
    assert!(!logs.contains("s3cret"), "logs leaked credential: {logs}");
    assert!(!err.contains("s3cret"), "error leaked credential: {err}");
    assert!(
        logs.contains("***@"),
        "logs missing redaction marker: {logs}"
    );
}

/// #431: a real *transport* failure (not an API status) must not put reqwest's
/// own `Display` — which appends ` for url (…)` verbatim — into either the log
/// stream or the returned `ObsError`, whose `Http` variant is transparent down
/// to `reqwest::Error`.
///
/// OBS has no origin guard, so a credentialed API base reaches `send`; that is
/// what makes this the datasource where the credential path is actually
/// reachable. reqwest strips first-hop userinfo into a Basic-auth header
/// (`RequestBuilder::new` → `extract_authority`), so the password is already
/// absent from the error URL — the `s3cret` assertions are belt-and-braces, and
/// the substring that is genuinely red without the fix is `" for url ("`.
#[tokio::test]
async fn transport_error_logs_and_error_carry_no_reqwest_url() {
    // Loopback discard port (RFC 863): nothing listens, so `send` fails.
    let client = ObsClient::new(
        "http://alice:s3cret@127.0.0.1:9",
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds");

    let mut err = String::new();
    let mut dbg = String::new();
    let logs = capture_logs(|| async {
        let e = client
            .get("request/1", &[])
            .await
            .expect_err("connection refused is an error");
        err = format!("{e}");
        dbg = format!("{e:?}");
    })
    .await;

    // Assert on the *failure* line specifically. The unconditional pre-send
    // debug line already carries a sanitized URL, so a whole-capture anchor
    // would stay green even if this line were deleted outright. The selector
    // names mtui's own line exactly: `capture_logs` records every event on the
    // thread, including hyper's pool chatter, whose content varies with
    // connection state.
    let failure = logs
        .lines()
        .find(|l| l.starts_with("OBS GET") && l.contains("failed:"))
        .unwrap_or_else(|| panic!("the transport-failure arm never ran: {logs}"));
    assert!(
        !failure.contains(" for url ("),
        "log rendered reqwest's URL: {failure}"
    );
    assert!(
        !failure.contains("s3cret"),
        "log leaked credential: {failure}"
    );
    // Dropping reqwest's URL must not leave the operator with no host at all.
    assert!(
        failure.contains("***@127.0.0.1"),
        "log lost its sanitized URL context: {failure}"
    );

    assert!(
        !err.contains(" for url ("),
        "error rendered reqwest's URL: {err}"
    );
    assert!(!err.contains("s3cret"), "error leaked credential: {err}");
    assert!(!dbg.contains("s3cret"), "debug leaked credential: {dbg}");
    // Positive anchor: only the URL is dropped, never the failure kind — an
    // over-stripping conversion must not pass this test.
    assert!(
        err.contains("error sending request"),
        "error lost the failure kind: {err}"
    );
}

/// #431: the between-calls budget message interpolated the raw URL, so an
/// exhausted budget against a credentialed base leaked the password verbatim —
/// the one site where the credential (not merely the URL) was reachable.
#[tokio::test]
async fn budget_timeout_message_redacts_url_credentials() {
    let client = ObsClient::new(
        "http://alice:s3cret@127.0.0.1:9",
        Duration::from_secs(0),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .expect("client builds");

    let err = client
        .get("request/1", &[])
        .await
        .expect_err("exhausted budget aborts");
    let ObsError::Timeout(msg) = &err else {
        panic!("expected ObsError::Timeout, got {err:?}");
    };
    assert!(!msg.contains("s3cret"), "timeout leaked credential: {msg}");
    assert!(
        msg.contains("***@127.0.0.1"),
        "timeout dropped the URL context: {msg}"
    );
}
