//! Integration tests for the native OBS HTTP transport against a real HTTP
//! transport (`wiremock`).
//!
//! Ports the behavioral core of upstream `tests/test_obs_client.py`: GET/POST
//! success (headers + XML body), the `<status><summary>` error envelope → typed
//! [`ObsError::Api`], the empty-summary fallback for a non-XML body, and the
//! coarse between-calls budget abort. Auth is [`NoAuth`] — the SSH-signature
//! signer lands in a later subtask (G1c).
//!
//! Deviation from upstream: `test_tls_error_*` forges a `requests.SSLError` via
//! the `responses` library, which has no wiremock analogue (a mock server can't
//! forge a TLS handshake failure). The transport-error branch is instead
//! covered by pointing the client at an unroutable/closed endpoint so `request`
//! returns [`ObsError::Http`]; the `is_ssl_verification_error` mapping itself is
//! unit-tested in `src/http.rs`.

use std::sync::Arc;
use std::time::Duration;

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
    // first call checks it, mirroring upstream's `client._deadline = past`.
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

/// Capture the `message` field of every tracing event emitted by `f`.
///
/// A thread-local subscriber works under `#[tokio::test]`'s current-thread
/// runtime. Mirrors the capture helper in `obs_oscrc`/`gitea`.
async fn capture_logs<F, Fut>(f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use std::fmt::Write as _;
    use std::sync::{Arc as StdArc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    struct CaptureLayer(StdArc<Mutex<Vec<String>>>);
    struct MessageVisitor(String);
    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.0, "{value:?}");
            }
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut v = MessageVisitor(String::new());
            event.record(&mut v);
            self.0.lock().unwrap().push(v.0);
        }
    }

    let records = StdArc::new(Mutex::new(Vec::new()));
    let sub = Registry::default().with(CaptureLayer(records.clone()));
    let guard = tracing::subscriber::set_default(sub);
    f().await;
    drop(guard);
    records.lock().unwrap().join("\n")
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
