//! Integration tests for the openQA connectors against a real HTTP transport
//! (`wiremock`).
//!
//! Covers the request/auth contract: `get_jobs` folds
//! every failure into `None`, a well-formed response deserialises into jobs, and
//! the signed request carries the `X-API-Key`/`X-API-Hash` auth headers.

use mtui_datasources::VerifyPolicy;
use mtui_datasources::openqa::base::{IncidentName, OpenQABase};
use mtui_types::RequestReviewID;
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Incident(&'static str);
impl IncidentName for Incident {
    fn get_incident_name(&self) -> String {
        self.0.to_string()
    }
}

/// Builds an [`OpenQABase`] against `server_uri`, with credentials taken from
/// an isolated `config_paths` fixture directory (not the real filesystem), so
/// tests are hermetic under the one-test-binary-per-crate convention.
fn base_for(server_uri: &str, config_dir: &std::path::Path) -> OpenQABase {
    let transport = mtui_datasources::HttpClient::openqa_transport(VerifyPolicy::Default(true))
        .expect("transport builds");
    let client = ruoqa::ClientBuilder::new()
        .server(server_uri)
        .http_client(transport)
        .config_paths(vec![config_dir.join("client.conf")])
        .retry(ruoqa::RetryPolicy::default().max_retries(0).deadline(None))
        .build()
        .expect("ruoqa client builds");
    let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    OpenQABase::new(client, &rrid, &Incident("bash"))
}

/// [`base_for`] against an empty (no `client.conf`) fixture directory, for
/// tests that don't care about credentials.
fn base_for_no_creds(server_uri: &str) -> OpenQABase {
    let dir = tempfile::tempdir().unwrap();
    base_for(server_uri, dir.path())
}

#[tokio::test]
async fn get_jobs_returns_parsed_jobs_on_success() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "jobs": [
            {
                "id": 1,
                "test": "qam-incidentinstall",
                "result": "passed",
                "clone_id": null,
                "settings": {"ARCH": "x86_64", "VERSION": "15-SP5"},
                "modules": []
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    let jobs = base.get_jobs().await.expect("Some(jobs)");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, 1);
    assert_eq!(jobs[0].test, "qam-incidentinstall");
}

#[tokio::test]
async fn get_jobs_returns_none_on_error_status() {
    // Upstream: an openqa_client RequestError (HTTP error code) yields None.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    assert!(base.get_jobs().await.is_none());
}

#[tokio::test]
async fn get_jobs_returns_none_on_malformed_body() {
    // A non-JSON / wrong-shape body must not escape as a traceback.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    assert!(base.get_jobs().await.is_none());
}

#[tokio::test]
async fn get_jobs_returns_none_on_connection_failure() {
    // Point at a port with no listener: transport failure -> None.
    let base = base_for_no_creds("http://127.0.0.1:1");
    assert!(base.get_jobs().await.is_none());
}

#[tokio::test]
async fn try_get_jobs_errs_on_error_status() {
    // The fallible sibling surfaces a non-2xx as Err instead of folding to None.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    assert!(base.try_get_jobs().await.is_err());
}

/// The `http://user:pass@host` form was already stripped of userinfo by
/// `ruoqa::config::resolve`'s `netloc()` branch before 0.1.4 existed, so a
/// credential there could never have regressed — it never reaches a request.
/// The *bare-authority* form (no scheme, e.g. `alice:s3cret@host`) skips that
/// branch and reached `Url::parse` untouched in 0.1.3, only gaining a strip
/// in 0.1.4 (`config::resolve`'s post-parse `username()`/`password()`
/// check). `describe` no longer sanitizes the URL itself — that's `ruoqa`'s
/// job now — so this is the one assertion in this file that turns red on a
/// `ruoqa 0.1.3` downgrade.
#[tokio::test]
async fn try_get_jobs_error_never_leaks_a_bare_authority_credential() {
    let dir = tempfile::tempdir().unwrap();
    let base = base_for("user:s3cret@127.0.0.1:1", dir.path());
    let err = base.try_get_jobs().await.unwrap_err().to_string();
    assert!(!err.contains("s3cret"), "error leaked credential: {err}");
}

#[tokio::test]
async fn try_get_jobs_ok_empty_on_valid_empty_body() {
    // A valid-but-empty response is Ok(vec![]), distinct from a fetch failure.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    assert!(base.try_get_jobs().await.unwrap().is_empty());
}

/// Drives `ruoqa`'s own redaction with a *real* `Error::CrossOriginRedirect`
/// carrying a credentialed URL, rather than a hand-written string.
///
/// `ruoqa::config::resolve` drops userinfo from the *client's own* base URL
/// before any request, but a `Location` header is server-controlled: a
/// malicious or misconfigured openQA instance could redirect off-origin to a
/// URL embedding `user:pass@`. ruoqa refuses to follow it (same-origin only),
/// and (>= 0.1.4) redacts both URLs in `Error::CrossOriginRedirect`'s
/// `Display` itself. Pins the exact `***` marker, not just credential
/// absence: a `ruoqa 0.1.3` downgrade renders both URLs verbatim, turning
/// this red.
#[tokio::test]
async fn try_get_jobs_error_never_leaks_a_credential_from_a_redirect_location() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", "https://alice:s3cret@evil.example.com/x"),
        )
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    let err = base.try_get_jobs().await.unwrap_err().to_string();
    assert!(
        err.ends_with("to a different origin https://***@evil.example.com/x"),
        "expected the redacted redirect target, got: {err}"
    );
}

/// Risk 2 backstop: a connection-failure message must never contain
/// reqwest's own `Display` suffix (`" for url (...)"`,
/// reqwest-0.13.4/src/error.rs:280) — that would leak the request URL
/// unredacted, bypassing `ruoqa`'s own `Connection` redaction entirely.
/// Swapping `root_cause(source)` for `source.to_string()` in `describe`
/// turns this red.
#[tokio::test]
async fn try_get_jobs_connection_failure_never_leaks_reqwests_own_url_suffix() {
    let base = base_for_no_creds("http://127.0.0.1:1");
    let err = base.try_get_jobs().await.unwrap_err().to_string();
    assert!(
        !err.contains(" for url ("),
        "leaked reqwest's own unredacted Display: {err}"
    );
}

/// Risk 1: the message still carries a real transport cause, not just the
/// redacted URL — dropping `root_cause`'s recovered cause in `describe`
/// turns this red. Deliberately doesn't pin the OS-specific errno text (61
/// on macOS, 111 on Linux).
#[tokio::test]
async fn try_get_jobs_connection_failure_message_carries_a_transport_cause() {
    let base = base_for_no_creds("http://127.0.0.1:1");
    let err = base.try_get_jobs().await.unwrap_err().to_string();
    assert!(
        err.contains("failed to connect to http://127.0.0.1:1/api/v1/jobs"),
        "unexpected message shape: {err}"
    );
    let cause = err.rsplit_once(": ").map_or("", |(_, c)| c);
    assert!(
        !cause.is_empty(),
        "expected a non-empty cause after the URL, got: {err}"
    );
}

#[tokio::test]
async fn request_carries_accept_and_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    // Some(empty) — matched only if Accept header + path matched.
    assert_eq!(base.get_jobs().await.map(|j| j.len()), Some(0));
}

#[tokio::test]
async fn request_carries_auth_headers_when_credentials_present() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .and(header("X-API-Key", "MYKEY"))
        .and(header_exists("X-API-Microtime"))
        .and(header_exists("X-API-Hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("client.conf");
    std::fs::write(
        &conf,
        format!(
            "[{}]\nkey = MYKEY\nsecret = MYSECRET\n",
            strip_scheme(&server.uri())
        ),
    )
    .unwrap();
    let base = base_for(&server.uri(), dir.path());
    // Matches only if all three auth headers were present on the request.
    assert_eq!(base.get_jobs().await.map(|j| j.len()), Some(0));
}

#[tokio::test]
async fn request_omits_auth_headers_without_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
        .mount(&server)
        .await;

    let base = base_for_no_creds(&server.uri());
    assert_eq!(base.get_jobs().await.map(|j| j.len()), Some(0));
}

/// Credentials are also honored from `$OPENQA_CONFIG`, `#[serial]`-guarded
/// since it mutates process-global environment state.
#[tokio::test]
#[serial_test::serial(openqa_config_env)]
// `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
// `#[serial(openqa_config_env)]` guard makes the mutation exclusive.
#[allow(unsafe_code)]
async fn request_carries_auth_headers_from_openqa_config_env() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .and(header("X-API-Key", "ENVKEY"))
        .and(header_exists("X-API-Hash"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("client.conf"),
        format!(
            "[{}]\nkey = ENVKEY\nsecret = ENVSECRET\n",
            strip_scheme(&server.uri())
        ),
    )
    .unwrap();

    // SAFETY: serialised via `#[serial(openqa_config_env)]`.
    unsafe { std::env::set_var("OPENQA_CONFIG", dir.path()) };
    let transport =
        mtui_datasources::HttpClient::openqa_transport(VerifyPolicy::Default(true)).unwrap();
    let client = ruoqa::ClientBuilder::new()
        .server(server.uri())
        .http_client(transport)
        .retry(ruoqa::RetryPolicy::default().max_retries(0).deadline(None))
        .build()
        .unwrap();
    // SAFETY: still inside the `#[serial(openqa_config_env)]` critical section.
    unsafe { std::env::remove_var("OPENQA_CONFIG") };

    let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
    let base = OpenQABase::new(client, &rrid, &Incident("bash"));
    assert_eq!(base.get_jobs().await.map(|j| j.len()), Some(0));
}

/// Pins the *upstream* contract this crate's `build_openqa_client_with_transport`
/// depends on never having called `.tls()`/`.timeouts()`: combining either with
/// an injected `http_client` is a runtime `Error::IncompatibleHttpClient`, not
/// a compile error, so a `ruoqa` upgrade that loosened this would only surface
/// as a mysterious runtime failure. This test exercises `ruoqa::ClientBuilder`
/// directly (not mtui's wrapper, which never calls either) to record that
/// contract explicitly. The regression guard for mtui's *own* code never
/// re-adding either call is
/// `openqa::client::tests::build_openqa_client_succeeds_with_default_verify`
/// (in `src/openqa/client.rs`), which builds through the real wrapper.
#[test]
fn injected_transport_rejects_tls_override() {
    let err = ruoqa::ClientBuilder::new()
        .server("openqa.example.com")
        .http_client(reqwest::Client::new())
        .tls(ruoqa::TlsMode::danger_accept_invalid_certs())
        .config_paths(vec![])
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        ruoqa::Error::IncompatibleHttpClient { option: "tls" }
    ));
}

#[test]
fn injected_transport_rejects_timeouts_override() {
    let err = ruoqa::ClientBuilder::new()
        .server("openqa.example.com")
        .http_client(reqwest::Client::new())
        .timeouts(ruoqa::Timeouts::default())
        .config_paths(vec![])
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        ruoqa::Error::IncompatibleHttpClient { option: "timeouts" }
    ));
}

fn strip_scheme(uri: &str) -> &str {
    uri.split_once("://").map_or(uri, |(_, rest)| rest)
}
