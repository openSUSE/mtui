//! Integration tests for the Gitea PR review-workflow connector against a real
//! HTTP transport (`wiremock`).
//!
//! Ports the behavioral core of upstream `tests/test_gitea.py`'s
//! `TestGiteaOperations`, `TestAssignmentStateMachine`, and `TestAssignee`: the
//! comment-driven assign/approve/reject state machine, the "re-requested review
//! supersedes a stale decision" rule, the assignment guards, and the request
//! failure / auth-header contract.
//!
//! wiremock matches by request shape rather than call order, so an operation
//! that issues several GETs to the same endpoint (each returning the same
//! comment snapshot) is modelled by one mounted GET plus one mounted POST —
//! exactly the states the ported Python cases set up.

use mtui_datasources::gitea::{Gitea, assign_marker};
use mtui_datasources::{HttpClient, VerifyPolicy};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USER: &str = "testuser";
const GROUP: &str = "qam-sle";

/// Build a Gitea client whose PR/comments endpoints resolve to `server`.
///
/// The mock server is mounted at `/api/v1/repos/owner/repo/pulls/1`, so the
/// derived comments endpoint is `/api/v1/repos/owner/repo/issues/1/comments`.
fn gitea_for(server: &MockServer) -> Gitea {
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    let pr_api = format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri());
    Gitea::with_client(http, "tok".to_string(), USER.to_string(), &pr_api, None)
}

const PR_PATH: &str = "/api/v1/repos/owner/repo/pulls/1";
const COMMENTS_PATH: &str = "/api/v1/repos/owner/repo/issues/1/comments";

fn ts(day: u32) -> String {
    format!("2024-01-{day:02}T00:00:00+00:00")
}

fn comment_json(id: i64, body: &str, day: u32) -> serde_json::Value {
    json!({ "id": id, "body": body, "updated_at": ts(day) })
}

/// Mount a GET on the comments endpoint returning `comments`.
async fn mount_comments(server: &MockServer, comments: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(COMMENTS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(comments))
        .mount(server)
        .await;
}

/// Mount a GET on the PR endpoint returning `requested_reviewers`.
async fn mount_pr_reviewers(server: &MockServer, reviewers: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "requested_reviewers": reviewers })),
        )
        .mount(server)
        .await;
}

/// Mount the POST on the comments endpoint (the "post a comment" sink).
async fn mount_post_comment(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(COMMENTS_PATH))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 999 })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn assign_success_when_review_requested_and_unassigned() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await; // no markers -> unassigned, not done
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;

    gitea_for(&server).assign(None, false).await.unwrap();

    // The POST carries an assignment marker for the session user.
    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(body.contains(&format!("assigned to user: {USER}")));
}

#[tokio::test]
async fn assign_force_posts_even_when_assigned_to_other() {
    let server = MockServer::start().await;
    // An assignment marker for alice: is_done sees an assign (not a decision).
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker("alice", GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;

    gitea_for(&server).assign(None, true).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(body.contains(&format!("assigned to user: {USER}")));
}

#[tokio::test]
async fn assign_without_force_refuses_when_assigned_to_other() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker("alice", GROUP), 1)]),
    )
    .await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;

    let err = gitea_for(&server).assign(None, false).await.unwrap_err();
    assert!(matches!(
        err,
        mtui_datasources::GiteaError::AssignInvalid { .. }
    ));
}

#[tokio::test]
async fn assign_no_review_raises() {
    let server = MockServer::start().await;
    mount_pr_reviewers(&server, json!([])).await;
    // No comments endpoint needed: has_review() short-circuits.

    let err = gitea_for(&server).assign(None, false).await.unwrap_err();
    assert!(matches!(err, mtui_datasources::GiteaError::NoReview(_)));
}

#[tokio::test]
async fn approve_uses_last_assignee() {
    let server = MockServer::start().await;
    // alice then the session user assigned -> last assignee is us.
    mount_comments(
        &server,
        json!([
            comment_json(1, &assign_marker("alice", GROUP), 1),
            comment_json(2, &assign_marker(USER, GROUP), 2),
        ]),
    )
    .await;
    mount_post_comment(&server).await;

    gitea_for(&server).approve(None).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    assert!(String::from_utf8_lossy(&posts[0].body).contains("LGTM"));
}

/// Request-count oracle (mtui-rs-0mop.8: deduplicate Gitea approval fetches).
///
/// A single happy-path `approve` fetches the comment snapshot **once** and
/// derives both the assignment state (`assign_state`) and the decision state
/// (`is_done_from`) from it — one comments GET, plus one POST. `is_done_from`
/// short-circuits before `has_review` here because no decision comment exists,
/// so the PR endpoint is not hit. The count fails the test if it drifts either
/// way (a regression back to the old double-fetch, or an unexpected extra call).
#[tokio::test]
async fn approve_request_count() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;

    gitea_for(&server).approve(None).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let comment_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == COMMENTS_PATH)
        .count();
    let pr_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == PR_PATH)
        .count();
    let posts = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    // Deduplicated: comments fetched once per approve; no PR GET (no decision);
    // one POST.
    assert_eq!(comment_gets, 1, "approve fetches comments once; see 0mop.8");
    assert_eq!(pr_gets, 0, "no decision comment -> no has_review PR GET");
    assert_eq!(posts, 1);
}

#[tokio::test]
async fn approve_after_rebuild_rerequest_posts_comment() {
    // A stale decline lingers, but the group's review is requested again ->
    // not done -> approve proceeds with a fresh LGTM.
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([
            comment_json(1, &assign_marker(USER, GROUP), 1),
            comment_json(2, &format!("@{GROUP}-review: decline"), 2),
        ]),
    )
    .await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;

    gitea_for(&server).approve(None).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    assert!(String::from_utf8_lossy(&posts[0].body).contains("LGTM"));
}

#[tokio::test]
async fn approve_when_already_decided_raises() {
    // A standing LGTM with no pending re-request blocks approve.
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([
            comment_json(1, &assign_marker(USER, GROUP), 1),
            comment_json(2, &format!("@{GROUP}-review: LGTM"), 2),
        ]),
    )
    .await;
    mount_pr_reviewers(&server, json!([])).await;

    let err = gitea_for(&server).approve(None).await.unwrap_err();
    assert!(matches!(err, mtui_datasources::GiteaError::NoReview(_)));
}

#[tokio::test]
async fn approve_when_not_assigned_raises() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;

    let err = gitea_for(&server).approve(None).await.unwrap_err();
    assert!(matches!(
        err,
        mtui_datasources::GiteaError::AssignInvalid { .. }
    ));
}

#[tokio::test]
async fn reject_posts_decline_with_reason() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;

    gitea_for(&server)
        .reject("broke boot", None, "see logs")
        .await
        .unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(body.contains("decline"));
    assert!(body.contains("Reason: broke boot"));
    assert!(body.contains("see logs"));
}

/// Request-count oracle (0mop.8): a happy-path `reject` fetches the comment
/// snapshot once (no decision -> no PR GET) and posts once.
#[tokio::test]
async fn reject_request_count() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;

    gitea_for(&server).reject("", None, "").await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let comment_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == COMMENTS_PATH)
        .count();
    let pr_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == PR_PATH)
        .count();
    let posts = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    assert_eq!(comment_gets, 1, "reject fetches comments once");
    assert_eq!(pr_gets, 0, "no decision comment -> no has_review PR GET");
    assert_eq!(posts, 1);
}

/// Request-count oracle (0mop.8): a happy-path `assign` (no `force`) issues one
/// PR GET (`has_review` review-requested guard), fetches the comment snapshot
/// **once** (feeding both `is_done_from` and the unassigned guard — no refetch),
/// and posts once. No decision comment means `has_review` runs only once (the
/// guard), not again from the decision path.
#[tokio::test]
async fn assign_request_count() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await; // unassigned, no decision
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;

    gitea_for(&server).assign(None, false).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let comment_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == COMMENTS_PATH)
        .count();
    let pr_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == PR_PATH)
        .count();
    let posts = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    assert_eq!(comment_gets, 1, "assign fetches comments once");
    assert_eq!(
        pr_gets, 1,
        "one has_review PR GET (the review-requested guard)"
    );
    assert_eq!(posts, 1);
}

#[tokio::test]
async fn unassign_when_not_assigned_raises() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;

    let err = gitea_for(&server).unassign(None).await.unwrap_err();
    assert!(matches!(
        err,
        mtui_datasources::GiteaError::AssignInvalid { .. }
    ));
}

#[tokio::test]
async fn comment_posts_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(COMMENTS_PATH))
        .and(body_string_contains("test comment body"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .mount(&server)
        .await;

    gitea_for(&server)
        .comment("test comment body")
        .await
        .unwrap();
}

#[tokio::test]
async fn get_hash_returns_head_sha() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "head": { "sha": "abc123def456" } })),
        )
        .mount(&server)
        .await;

    let sha = gitea_for(&server).get_hash().await.unwrap();
    assert_eq!(sha, "abc123def456");
}

#[tokio::test]
async fn request_failure_raises_failed_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "not found" })))
        .mount(&server)
        .await;

    let err = gitea_for(&server).get_hash().await.unwrap_err();
    assert!(matches!(err, mtui_datasources::GiteaError::FailedCall(_)));
}

#[tokio::test]
async fn request_sends_authorization_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .and(header("Authorization", "token tok"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "head": { "sha": "deadbeef" } })),
        )
        .mount(&server)
        .await;

    // Succeeds only if the Authorization header matched.
    assert_eq!(gitea_for(&server).get_hash().await.unwrap(), "deadbeef");
}

#[tokio::test]
async fn assignee_returns_current_user() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker("alice", GROUP), 1)]),
    )
    .await;

    assert_eq!(
        gitea_for(&server).assignee().await.unwrap(),
        Some("alice".to_string())
    );
}

/// Capture the `message` field of every tracing event emitted by `f`.
///
/// A thread-local subscriber (`with_default`) works here because `#[tokio::test]`
/// drives a current-thread runtime, so the awaited request's events fire on this
/// thread. Mirrors the capture helper in `obs_oscrc`.
async fn capture_logs<F, Fut>(f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    struct CaptureLayer(Arc<Mutex<Vec<String>>>);
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

    let records = Arc::new(Mutex::new(Vec::new()));
    let sub = Registry::default().with(CaptureLayer(records.clone()));
    let guard = tracing::subscriber::set_default(sub);
    f().await;
    drop(guard);
    records.lock().unwrap().join("\n")
}

#[tokio::test]
async fn request_logs_and_error_redact_url_credentials() {
    let server = MockServer::start().await;
    // Any request to the comments endpoint 404s, driving both the failure `warn!`
    // and the `FailedCall` error through the sanitizing path.
    Mock::given(method("GET"))
        .and(path(COMMENTS_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // Embed `user:s3cret@` credentials in the target authority.
    let authority = server.uri().replace("http://", "http://user:s3cret@");
    let pr_api = format!("{authority}/api/v1/repos/owner/repo/pulls/1");
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    let client = Gitea::with_client(http, "tok".to_string(), USER.to_string(), &pr_api, None);

    let mut err = String::new();
    let logs = capture_logs(|| async {
        let e = client.assignee().await.unwrap_err();
        err = format!("{e}");
    })
    .await;

    // Neither the captured debug/warn logs nor the surfaced error leak the
    // password, but the host is preserved for diagnosis.
    assert!(!logs.contains("s3cret"), "logs leaked credential: {logs}");
    assert!(!err.contains("s3cret"), "error leaked credential: {err}");
    assert!(
        logs.contains("***@"),
        "logs missing redaction marker: {logs}"
    );
}

#[tokio::test]
async fn assignee_none_when_unassigned() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;

    assert_eq!(gitea_for(&server).assignee().await.unwrap(), None);
}
