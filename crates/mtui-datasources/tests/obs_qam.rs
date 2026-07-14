//! Integration tests for the native QAM operations (`mtui_datasources::obs::qam`).
//!
//! Ported 1:1 from upstream `tests/test_obs_qam.py`. Python mocks HTTP with
//! `responses`; this port uses `wiremock` — the OBS API base backs the
//! `ObsClient` calls and a second `wiremock` server backs the `qam.suse.de`
//! reports host that `preconditions` fetches (with no OBS auth). Pinned query
//! params and bodies are asserted from `received_requests()`.

use std::sync::Arc;
use std::time::Duration;

use mtui_config::SslVerify;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::obs::client::{NoAuth, ObsClient};
use mtui_datasources::obs::qam;
use mtui_types::RequestReviewID;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "qamuser";

fn rrid() -> RequestReviewID {
    RequestReviewID::parse("SUSE:Maintenance:1:56789").unwrap()
}

fn pi_rrid() -> RequestReviewID {
    // SLFO kind -> skips preconditions.
    RequestReviewID::parse("SUSE:SLFO:1.1:70000").unwrap()
}

fn client_for(server: &MockServer) -> ObsClient {
    ObsClient::new(
        &server.uri(),
        Duration::from_secs(180),
        VerifyPolicy::Default(true),
        Arc::new(NoAuth),
    )
    .unwrap()
}

fn request_xml(state: &str, reviews: &str) -> String {
    format!(
        "<request id='56789'><state name='{state}'/>\
         <action type='maintenance_release'>\
         <source project='SUSE:Maintenance:1' package='p'/></action>\
         {reviews}</request>"
    )
}

fn group_review(group: &str, state: &str, events: &[(&str, &str, &str)]) -> String {
    let hist: String = events
        .iter()
        .map(|(w, t, d)| {
            format!("<history who='{w}' when='{t}'><description>{d}</description></history>")
        })
        .collect();
    format!("<review state='{state}' by_group='{group}'>{hist}</review>")
}

const ACCEPT: &str = "Review got accepted";
// The testreport log path is `{reports_url}/{rrid}/log` where `{rrid}` is the
// full RRID string (upstream `_log_url` uses the RRID's `__str__`).
const LOG_PATH: &str = "/SUSE:Maintenance:1:56789/log";

// A GET testreport log mock on the reports server.
async fn mount_log(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path(LOG_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

async fn mount_log_status(server: &MockServer, status: u16) {
    Mock::given(method("GET"))
        .and(path(LOG_PATH))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

// A GET request/56789 mock on the OBS API server.
async fn mount_get_request(server: &MockServer, body: String) {
    Mock::given(method("GET"))
        .and(path("/request/56789"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

// A POST request/56789 mock (assign/unassign/changereviewstate).
async fn mount_post_request(server: &MockServer, id: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/request/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(server)
        .await;
}

// A GET request (collection) mock.
async fn mount_collection(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/request"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

async fn mount_group(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/group"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

/// The parsed query of the first recorded call matching method + URL path.
async fn query_of(
    server: &MockServer,
    verb: wiremock::http::Method,
    url_path: &str,
) -> Vec<(String, String)> {
    let reqs = server.received_requests().await.unwrap();
    let call = reqs
        .into_iter()
        .find(|r| r.method == verb && r.url.path() == url_path)
        .unwrap_or_else(|| panic!("no {verb:?} call to {url_path} was recorded"));
    call.url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn query_val<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The body of the last recorded POST to `url_path`.
async fn last_post_body(server: &MockServer, url_path: &str) -> String {
    let reqs = server.received_requests().await.unwrap();
    let call = reqs
        .into_iter()
        .rfind(|r| r.method == wiremock::http::Method::POST && r.url.path() == url_path)
        .unwrap_or_else(|| panic!("no POST to {url_path}"));
    String::from_utf8_lossy(&call.body).into_owned()
}

// --------------------------------------------------------------------------- //
// comment                                                                      //
// --------------------------------------------------------------------------- //
#[tokio::test]
async fn comment_posts_raw_unprefixed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/comments/request/56789"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&server)
        .await;
    qam::comment(&client_for(&server), &rrid(), "looks good")
        .await
        .unwrap();
    let body = last_post_body(&server, "/comments/request/56789").await;
    assert_eq!(body, "looks good");
}

#[tokio::test]
async fn comment_empty_refused() {
    let server = MockServer::start().await;
    let err = qam::comment(&client_for(&server), &rrid(), "   ")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("empty comment"), "{err}");
}

// --------------------------------------------------------------------------- //
// assign                                                                       //
// --------------------------------------------------------------------------- //
#[tokio::test]
async fn assign_explicit_group() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    mount_collection(&api, "<collection/>").await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();

    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "cmd"), Some("assignreview"));
    assert_eq!(query_val(&q, "reviewer"), Some(USER));
    assert_eq!(query_val(&q, "by_group"), Some("qam-sle"));
}

#[tokio::test]
async fn assign_auto_infers_single_group() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews =
        "<review state='new' by_group='qam-sle'/><review state='new' by_group='qam-cloud'/>";
    mount_get_request(&api, request_xml("review", reviews)).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    mount_group(&api, "<directory><entry name=\"qam-sle\"/></directory>").await;
    mount_collection(&api, "<collection/>").await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap();

    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "by_group"), Some("qam-sle"));
}

#[tokio::test]
async fn assign_auto_infer_ambiguous_refused() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews =
        "<review state='new' by_group='qam-sle'/><review state='new' by_group='qam-cloud'/>";
    mount_get_request(&api, request_xml("review", reviews)).await;
    mount_group(
        &api,
        "<directory><entry name=\"qam-sle\"/><entry name=\"qam-cloud\"/></directory>",
    )
    .await;

    let err = qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("auto-infer a single"), "{err}");
}

#[tokio::test]
async fn assign_refused_when_not_open() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("accepted", "")).await;
    let err = qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not open for review"), "{err}");
}

#[tokio::test]
async fn assign_accepts_state_new() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("new", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    mount_collection(&api, "<collection/>").await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();
    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "by_group"), Some("qam-sle"));
}

#[tokio::test]
async fn assign_refused_when_no_testreport() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log_status(&reports, 404).await;

    let err = qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no testreport"), "{err}");
}

#[tokio::test]
async fn assign_previous_reject_refused() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    let declined = "<collection><request id='9'><state name='declined'/>\
         <review state='declined' by_group='qam-sle'/>\
         <review state='declined' by_user='someone-else'/></request></collection>";
    mount_collection(&api, declined).await;

    let err = qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("previously declined"), "{err}");
}

#[tokio::test]
async fn assign_previous_reject_proceeds_when_user_was_prior_reviewer() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    let declined = format!(
        "<collection><request id='9'><state name='declined'/>\
         <review state='declined' by_group='qam-sle'/>\
         <review state='declined' by_user='{USER}'/></request></collection>"
    );
    mount_collection(&api, &declined).await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn assign_previous_reject_proceeds_when_none_declined() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    let open_related = "<collection><request id='9'><state name='review'/>\
         <review state='new' by_group='qam-sle'/></request></collection>";
    mount_collection(&api, open_related).await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn assign_pins_get_queries() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews = "<review state='new' by_group='qam-sle'/>";
    mount_get_request(&api, request_xml("review", reviews)).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    mount_group(&api, "<directory><entry name=\"qam-sle\"/></directory>").await;
    mount_collection(&api, "<collection/>").await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap();

    let get_req = query_of(&api, wiremock::http::Method::GET, "/request/56789").await;
    assert_eq!(query_val(&get_req, "withfullhistory"), Some("1"));
    let get_group = query_of(&api, wiremock::http::Method::GET, "/group").await;
    assert_eq!(query_val(&get_group, "login"), Some(USER));
}

#[tokio::test]
async fn assign_previous_reject_ignores_non_qam_declined() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    let non_qam = "<collection><request id='9'><state name='declined'/>\
         <review state='declined' by_group='maintenance-team'/></request></collection>";
    mount_collection(&api, non_qam).await;
    mount_post_request(&api, "56789").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();
    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "by_group"), Some("qam-sle"));
}

#[tokio::test]
async fn assign_skips_preconditions_for_pi() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(request_xml("review", "")))
        .mount(&api)
        .await;
    mount_post_request(&api, "70000").await;

    qam::assign(
        &client_for(&api),
        &reports.uri(),
        &SslVerify::Enabled,
        &pi_rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap();

    // Only the request GET and the assign POST — no testreport / collection.
    assert_eq!(api.received_requests().await.unwrap().len(), 2);
    assert!(reports.received_requests().await.unwrap().is_empty());
}

// --------------------------------------------------------------------------- //
// unassign                                                                     //
// --------------------------------------------------------------------------- //
#[tokio::test]
async fn unassign_reverts_inferred_group() {
    let api = MockServer::start().await;
    let reviews = group_review(
        "qam-sle",
        "accepted",
        &[(USER, "2020-01-01T00:00:00", ACCEPT)],
    );
    mount_get_request(&api, request_xml("review", &reviews)).await;
    mount_post_request(&api, "56789").await;

    qam::unassign(&client_for(&api), &rrid(), USER, &[])
        .await
        .unwrap();
    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "revert"), Some("1"));
    assert_eq!(query_val(&q, "by_group"), Some("qam-sle"));
}

#[tokio::test]
async fn unassign_reverts_explicit_group_over_inferred() {
    let api = MockServer::start().await;
    let reviews = group_review(
        "qam-sle",
        "accepted",
        &[(USER, "2020-01-01T00:00:00", ACCEPT)],
    );
    mount_get_request(&api, request_xml("review", &reviews)).await;
    mount_post_request(&api, "56789").await;

    qam::unassign(&client_for(&api), &rrid(), USER, &["qam-cloud".to_owned()])
        .await
        .unwrap();
    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "revert"), Some("1"));
    assert_eq!(query_val(&q, "by_group"), Some("qam-cloud"));
}

#[tokio::test]
async fn unassign_refused_without_assignment() {
    let api = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    let err = qam::unassign(&client_for(&api), &rrid(), USER, &["qam-sle".to_owned()])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("holds no review assignment"),
        "{err}"
    );
}

// --------------------------------------------------------------------------- //
// approve                                                                      //
// --------------------------------------------------------------------------- //
#[tokio::test]
async fn approve_user_path_prefixed() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews = group_review(
        "qam-sle",
        "accepted",
        &[(USER, "2020-01-01T00:00:00", ACCEPT)],
    );
    mount_get_request(&api, request_xml("review", &reviews)).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;
    mount_post_request(&api, "56789").await;

    qam::approve(
        &client_for(&api),
        &reports.uri(),
        "https://qam.suse.de/reports",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap();

    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "newstate"), Some("accepted"));
    assert_eq!(query_val(&q, "by_user"), Some(USER));
    let body = last_post_body(&api, "/request/56789").await;
    assert!(body.starts_with("[oscqam] "), "{body}");
}

#[tokio::test]
async fn approve_group_refused() {
    let api = MockServer::start().await;
    let err = qam::approve(
        &client_for(&api),
        "http://unused",
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &["qam-sle".to_owned()],
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("group approval is not supported"),
        "{err}"
    );
}

#[tokio::test]
async fn approve_refused_when_not_assigned() {
    let api = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    let err = qam::approve(
        &client_for(&api),
        "http://unused",
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not assigned"), "{err}");
}

#[tokio::test]
async fn approve_refused_when_not_passed() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews = group_review(
        "qam-sle",
        "accepted",
        &[(USER, "2020-01-01T00:00:00", ACCEPT)],
    );
    mount_get_request(&api, request_xml("review", &reviews)).await;
    mount_log(&reports, "SUMMARY: FAILED\n").await;

    let err = qam::approve(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not PASSED"), "{err}");
}

#[tokio::test]
async fn approve_refused_when_summary_has_trailing_qualifier() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    let reviews = group_review(
        "qam-sle",
        "accepted",
        &[(USER, "2020-01-01T00:00:00", ACCEPT)],
    );
    mount_get_request(&api, request_xml("review", &reviews)).await;
    mount_log(&reports, "SUMMARY: PASSED with notes\n").await;

    let err = qam::approve(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not PASSED"), "{err}");
}

// --------------------------------------------------------------------------- //
// reject                                                                       //
// --------------------------------------------------------------------------- //
#[tokio::test]
async fn reject_writes_reason_and_declines() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: FAILED\ncomment: broken\n").await;
    let attr_path = "/source/SUSE:Maintenance:1/_attribute/MAINT:RejectReason";
    Mock::given(method("GET"))
        .and(path(attr_path))
        .respond_with(ResponseTemplate::new(200).set_body_string("<attributes/>"))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path(attr_path))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&api)
        .await;
    mount_post_request(&api, "56789").await;

    qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
        "not_fixed",
        "some message",
    )
    .await
    .unwrap();

    let attr_body = last_post_body(&api, attr_path).await;
    assert!(attr_body.contains("56789:not_fixed"), "{attr_body}");
    let q = query_of(&api, wiremock::http::Method::POST, "/request/56789").await;
    assert_eq!(query_val(&q, "newstate"), Some("declined"));
    let decline_body = last_post_body(&api, "/request/56789").await;
    assert!(decline_body.starts_with("[oscqam] "), "{decline_body}");
    // Parity: the -M message is not in the decline comment.
    assert!(!decline_body.contains("some message"), "{decline_body}");
}

#[tokio::test]
async fn reject_appends_to_existing_reject_reason() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: FAILED\ncomment: broken\n").await;
    let attr_path = "/source/SUSE:Maintenance:1/_attribute/MAINT:RejectReason";
    Mock::given(method("GET"))
        .and(path(attr_path))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<attributes><attribute name=\"RejectReason\" namespace=\"MAINT\">\
             <value>100:regression</value></attribute></attributes>",
        ))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path(attr_path))
        .respond_with(ResponseTemplate::new(200).set_body_string("<ok/>"))
        .mount(&api)
        .await;
    mount_post_request(&api, "56789").await;

    qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
        "not_fixed",
        "msg",
    )
    .await
    .unwrap();

    let posted = last_post_body(&api, attr_path).await;
    assert!(posted.contains("100:regression"), "{posted}"); // pre-existing preserved
    assert!(posted.contains("56789:not_fixed"), "{posted}"); // new appended
}

#[tokio::test]
async fn reject_refused_when_not_failed() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: PASSED\n").await;

    let err = qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
        "not_fixed",
        "",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not FAILED"), "{err}");
}

#[tokio::test]
async fn reject_refused_without_comment() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    mount_get_request(&api, request_xml("review", "")).await;
    mount_log(&reports, "SUMMARY: FAILED\n").await;

    let err = qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &rrid(),
        USER,
        &[],
        "not_fixed",
        "",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no comment"), "{err}");
}

#[tokio::test]
async fn reject_pi_skips_attribute_and_summary() {
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(request_xml("review", "")))
        .mount(&api)
        .await;
    mount_post_request(&api, "70000").await;

    qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &pi_rrid(),
        USER,
        &[],
        "not_fixed",
        "",
    )
    .await
    .unwrap();

    // Only request GET + decline POST; no testreport, no attribute calls.
    assert_eq!(api.received_requests().await.unwrap().len(), 2);
    assert!(reports.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reject_ignores_group() {
    // With -g given on a PI request, reject still proceeds by_user (2 calls).
    let api = MockServer::start().await;
    let reports = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/request/70000"))
        .respond_with(ResponseTemplate::new(200).set_body_string(request_xml("review", "")))
        .mount(&api)
        .await;
    mount_post_request(&api, "70000").await;

    qam::reject(
        &client_for(&api),
        &reports.uri(),
        "http://unused",
        &SslVerify::Enabled,
        &pi_rrid(),
        USER,
        &["qam-sle".to_owned()],
        "not_fixed",
        "",
    )
    .await
    .unwrap();

    let q = query_of(&api, wiremock::http::Method::POST, "/request/70000").await;
    assert_eq!(query_val(&q, "newstate"), Some("declined"));
    assert_eq!(query_val(&q, "by_user"), Some(USER));
}
