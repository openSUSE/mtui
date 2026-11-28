//! Integration tests for the Gitea PR review-workflow connector against a real
//! HTTP transport (`wiremock`).
//!
//! Covers the comment-driven assign/approve/reject state machine, the
//! "re-requested review supersedes a stale decision" rule, the assignment
//! guards and the request-failure / auth-header contract.
//!
//! wiremock matches by request shape rather than call order, so an operation
//! issuing several GETs to one endpoint is modelled by a single mounted GET
//! plus a mounted POST.

use super::log_capture::capture_logs;
use mtui_datasources::gitea::{Gitea, assign_marker, token_login};
use mtui_datasources::{GiteaError, HttpClient, VerifyPolicy};
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
    Gitea::with_client(
        http,
        "tok".to_string(),
        USER.to_string(),
        &pr_api,
        &server.uri(),
        None,
    )
    .expect("gitea client builds")
}

const PR_PATH: &str = "/api/v1/repos/owner/repo/pulls/1";
const COMMENTS_PATH: &str = "/api/v1/repos/owner/repo/issues/1/comments";
/// The authenticated-user lookup used to resolve the token owner's login.
const USER_PATH: &str = "/api/v1/user";

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

/// Mount the authenticated-user lookup so the token owner resolves to `login`.
///
/// A write op with no explicit user resolves its identity via
/// `GET /api/v1/user`, so mounting this makes the default path record the token
/// owner's login rather than the session-user fallback.
async fn mount_user(server: &MockServer, login: &str) {
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": login })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn assign_success_when_review_requested_and_unassigned() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await; // no markers -> unassigned, not done
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await; // token owner resolves to USER

    gitea_for(&server).assign(None, false).await.unwrap();

    // The POST carries an assignment marker for the resolved token owner.
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
    mount_user(&server, USER).await;

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
    mount_user(&server, USER).await;

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
    // alice then the token owner assigned -> last assignee is us.
    mount_comments(
        &server,
        json!([
            comment_json(1, &assign_marker("alice", GROUP), 1),
            comment_json(2, &assign_marker(USER, GROUP), 2),
        ]),
    )
    .await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await;

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

/// Request-count oracle: deduplicate Gitea approval fetches.
///
/// A happy-path `approve` fetches the comment snapshot **once** and derives
/// both `assign_state` and `is_done_from` from it — one comments GET plus one
/// POST, and no PR GET because `is_done_from` short-circuits with no decision
/// comment. The exact count fails on drift either way.
#[tokio::test]
async fn approve_request_count() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await;

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
    let user_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == USER_PATH)
        .count();
    let posts = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    // Deduplicated: comments fetched once per approve; no PR GET (no decision);
    // one user lookup to resolve the token owner; one POST.
    assert_eq!(comment_gets, 1, "approve fetches comments once; see 0mop.8");
    assert_eq!(pr_gets, 0, "no decision comment -> no has_review PR GET");
    assert_eq!(user_gets, 1, "token owner resolved once");
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
    mount_user(&server, USER).await;

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
    mount_user(&server, USER).await;

    let err = gitea_for(&server).approve(None).await.unwrap_err();
    assert!(matches!(err, mtui_datasources::GiteaError::NoReview(_)));
}

fn posts(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count()
}

/// `check_decidable` makes the checks `approve` and `reject` make and posts
/// nothing. Mutation caught: a POST inside it.
#[tokio::test]
async fn check_decidable_refuses_like_approve_and_never_posts() {
    let assigned = MockServer::start().await;
    mount_comments(
        &assigned,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&assigned).await;
    mount_user(&assigned, USER).await;
    gitea_for(&assigned).check_decidable(None).await.unwrap();
    assert_eq!(posts(&assigned.received_requests().await.unwrap()), 0);

    let unassigned = MockServer::start().await;
    mount_comments(&unassigned, json!([])).await;
    mount_post_comment(&unassigned).await;
    let err = gitea_for(&unassigned)
        .check_decidable(None)
        .await
        .unwrap_err();
    assert!(matches!(err, GiteaError::AssignInvalid { .. }), "{err:?}");

    let decided = MockServer::start().await;
    mount_comments(
        &decided,
        json!([
            comment_json(1, &assign_marker(USER, GROUP), 1),
            comment_json(2, &format!("@{GROUP}-review: LGTM"), 2),
        ]),
    )
    .await;
    mount_pr_reviewers(&decided, json!([])).await;
    mount_user(&decided, USER).await;
    let err = gitea_for(&decided).check_decidable(None).await.unwrap_err();
    assert!(matches!(err, GiteaError::NoReview(_)), "{err:?}");
}

async fn mount_head(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "head": { "sha": sha } })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn approve_if_head_posts_only_while_the_head_is_the_one_vetted() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await;
    mount_head(&server, "abc").await;

    gitea_for(&server)
        .approve_if_head(None, "abc")
        .await
        .unwrap();
    assert_eq!(posts(&server.received_requests().await.unwrap()), 1);

    let err = gitea_for(&server)
        .approve_if_head(None, "old")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GiteaError::HeadMoved { expected, actual }
            if expected == "old" && actual == "abc"),
        "{err:?}"
    );
    assert_eq!(
        posts(&server.received_requests().await.unwrap()),
        1,
        "the moved head posted nothing"
    );
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
    mount_user(&server, USER).await;

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

/// Request-count oracle: a happy-path `reject` fetches the comment snapshot
/// once (no decision, so no PR GET) and posts once.
#[tokio::test]
async fn reject_request_count() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker(USER, GROUP), 1)]),
    )
    .await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await;

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
    let user_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == USER_PATH)
        .count();
    let posts = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count();
    assert_eq!(comment_gets, 1, "reject fetches comments once");
    assert_eq!(pr_gets, 0, "no decision comment -> no has_review PR GET");
    assert_eq!(user_gets, 1, "token owner resolved once");
    assert_eq!(posts, 1);
}

/// Request-count oracle: a happy-path `assign` (no `force`) issues one PR GET
/// (the `has_review` guard), fetches the comment snapshot **once** for both
/// `is_done_from` and the unassigned guard, and posts once. With no decision
/// comment `has_review` runs only for the guard, not again from the decision
/// path.
#[tokio::test]
async fn assign_request_count() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await; // unassigned, no decision
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    mount_user(&server, USER).await;

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
    let user_gets = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET && r.url.path() == USER_PATH)
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
    assert_eq!(user_gets, 1, "token owner resolved once");
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
async fn pr_diff_gets_the_dot_diff_url_as_text_with_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{PR_PATH}.diff")))
        .and(header("Authorization", "token tok"))
        .and(header("Accept", "text/plain"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff --git a/x b/x\n"))
        .expect(1)
        .mount(&server)
        .await;

    let diff = gitea_for(&server).pr_diff().await.unwrap();
    assert_eq!(diff, "diff --git a/x b/x\n");
}

#[tokio::test]
async fn pr_diff_failure_raises_failed_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{PR_PATH}.diff")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = gitea_for(&server).pr_diff().await.unwrap_err();
    assert!(matches!(err, mtui_datasources::GiteaError::FailedCall(_)));
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

// --- token-owner login resolution -------

/// With no explicit user, the assignment marker records the *token owner's*
/// Gitea login (from `GET /api/v1/user`), not the local session user.
#[tokio::test]
async fn assign_records_resolved_token_owner_not_session_user() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    // Token owner is "gitea_bot", which differs from the session user (USER).
    mount_user(&server, "gitea_bot").await;

    gitea_for(&server).assign(None, false).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(body.contains("assigned to user: gitea_bot"), "{body}");
    assert!(
        !body.contains(&format!("assigned to user: {USER}")),
        "{body}"
    );
}

/// When the user lookup fails (non-2xx), the acting identity falls back to the
/// session user so the review action still completes.
#[tokio::test]
async fn assign_falls_back_to_session_user_when_lookup_fails() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    // The user endpoint errors -> fall back to the session user.
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    gitea_for(&server).assign(None, false).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(
        body.contains(&format!("assigned to user: {USER}")),
        "{body}"
    );
}

/// A payload missing a (non-empty) `login` also falls back to the session user.
#[tokio::test]
async fn assign_falls_back_when_login_absent() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "" })))
        .mount(&server)
        .await;

    gitea_for(&server).assign(None, false).await.unwrap();

    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1);
    let body = String::from_utf8_lossy(&posts[0].body);
    assert!(
        body.contains(&format!("assigned to user: {USER}")),
        "{body}"
    );
}

/// An explicit `other` user bypasses the token-owner lookup entirely.
#[tokio::test]
async fn explicit_user_skips_token_owner_lookup() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pr_reviewers(&server, json!([{ "login": "qam-sle-review" }])).await;
    mount_post_comment(&server).await;
    // Any hit here would mean an unnecessary lookup was made.
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "gitea_bot" })))
        .expect(0)
        .mount(&server)
        .await;

    gitea_for(&server)
        .assign(Some("carol"), false)
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
    assert!(body.contains("assigned to user: carol"), "{body}");
}

// --- token_login: the account lookup without a PR ---

fn plain_http() -> HttpClient {
    HttpClient::new(VerifyPolicy::Default(true)).expect("client builds")
}

/// The login comes from `GET /api/v1/user` under the trusted base (a
/// trailing slash on it included), with the token in the header.
#[tokio::test]
async fn token_login_reads_the_token_owner() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .and(header("Authorization", "token tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "gitea_bot" })))
        .expect(1)
        .mount(&server)
        .await;

    let base = format!("{}/", server.uri());
    let login = token_login(plain_http(), "tok", &base).await.unwrap();
    assert_eq!(login, "gitea_bot");
}

/// A failed lookup is an error naming the URL and status; neither the error
/// nor the log carries the token.
#[tokio::test]
async fn token_login_failure_names_the_call_but_not_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let mut err = String::new();
    let logs = capture_logs(|| async {
        let e = token_login(plain_http(), "s3cr3t-token", &server.uri())
            .await
            .unwrap_err();
        assert!(matches!(e, GiteaError::FailedCall(_)), "{e}");
        err = e.to_string();
    })
    .await;
    assert!(err.contains(USER_PATH) && err.contains("500"), "{err}");
    assert!(!err.contains("s3cr3t-token"), "{err}");
    assert!(logs.contains(USER_PATH), "the failure is logged: {logs}");
    assert!(!logs.contains("s3cr3t-token"), "{logs}");
}

/// A reply naming no login is an error, not an empty name to filter on.
#[tokio::test]
async fn token_login_refuses_a_reply_without_a_login() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(USER_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "" })))
        .mount(&server)
        .await;

    let err = token_login(plain_http(), "tok", &server.uri())
        .await
        .unwrap_err();
    assert!(matches!(err, GiteaError::FailedCall(_)), "{err}");
}

/// Without a token nothing is sent; an untrusted base is refused before send.
#[tokio::test]
async fn token_login_sends_nothing_without_a_token_or_to_an_untrusted_base() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "x" })))
        .expect(0)
        .mount(&server)
        .await;

    let err = token_login(plain_http(), "", &server.uri())
        .await
        .unwrap_err();
    assert!(matches!(err, GiteaError::MissingToken), "{err}");
    let plaintext = server.uri().replace("127.0.0.1", "gitea.example.invalid");
    let err = token_login(plain_http(), "tok", &plaintext)
        .await
        .unwrap_err();
    assert!(matches!(err, GiteaError::UntrustedOrigin(_)), "{err}");
}

/// The origin guard refuses a credentialed PR URL *before* any request is sent,
/// and both the refusal log and the resulting error are sanitized.
///
/// This never reaches the transport, so it is no evidence about transport-error
/// rendering; that arm is covered by
/// `transport_failure_log_carries_no_reqwest_url` below.
#[tokio::test]
async fn credentialed_pr_url_is_refused_before_send_and_redacted() {
    let server = MockServer::start().await;
    // "Before send" is half the claim, so assert it: any request at all would
    // mean the guard ran too late.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;

    // A userinfo-bearing PR URL is refused before any request is sent, driving
    // both the refusal `warn!` and the `UntrustedOrigin` error through the
    // sanitizing path.
    let authority = server.uri().replace("http://", "http://user:s3cret@");
    let pr_api = format!("{authority}/api/v1/repos/owner/repo/pulls/1");
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    // Trust the (loopback) mock origin; the PR URL differs only by userinfo.
    let client = Gitea::with_client(
        http,
        "tok".to_string(),
        USER.to_string(),
        &pr_api,
        &server.uri(),
        None,
    )
    .expect("gitea client builds");

    let mut err = String::new();
    let mut kind_is_untrusted_origin = false;
    let logs = capture_logs(|| async {
        let e = client.assignee().await.unwrap_err();
        kind_is_untrusted_origin = matches!(e, GiteaError::UntrustedOrigin(_));
        err = format!("{e}");
    })
    .await;

    // "Refused by the origin guard" is the other half: a transport failure
    // would also produce a credential-free message, for the wrong reason.
    assert!(
        kind_is_untrusted_origin,
        "expected GiteaError::UntrustedOrigin, got {err}"
    );
    // No captured log or surfaced error may leak the password, but the host
    // stays for diagnosis.
    assert!(!logs.contains("s3cret"), "logs leaked credential: {logs}");
    assert!(!err.contains("s3cret"), "error leaked credential: {err}");
    assert!(
        logs.contains("***@"),
        "logs missing redaction marker: {logs}"
    );
}

/// #431: the *transport* arm, which the origin-guard case above never reaches.
/// Rendering the raw `reqwest::Error` appended its unredacted request URL
/// (` for url (…)`); the line must carry mtui's own sanitized URL instead.
///
/// A credentialed URL cannot be driven here: the origin guard rejects userinfo
/// in the configured base *and* in every request URL before send, so for Gitea
/// the enforceable invariant is URL disclosure, not credential disclosure.
#[tokio::test]
async fn transport_failure_log_carries_no_reqwest_url() {
    // Loopback discard port (RFC 863): trusted by the origin guard, so the
    // request reaches `send`, where nothing is listening.
    let client = gitea_with_trust("http://127.0.0.1:9", "http://127.0.0.1:9");

    let logs = capture_logs(|| async {
        client.assignee().await.expect_err("connection refused");
    })
    .await;

    // Anti-vacuity: this line only exists on the transport arm, so finding it
    // proves the origin guard did not short-circuit the request.
    let failure = logs
        .lines()
        .find(|l| l.starts_with("API call to Gitea"))
        .unwrap_or_else(|| panic!("transport arm never ran: {logs}"));
    assert!(
        !failure.contains(" for url ("),
        "log rendered reqwest's URL: {failure}"
    );
    assert!(
        failure.contains("127.0.0.1:9"),
        "log lost its URL context: {failure}"
    );
}

#[tokio::test]
async fn assignee_none_when_unassigned() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;

    assert_eq!(gitea_for(&server).assignee().await.unwrap(), None);
}

// --- token-origin restriction ---

/// Build a Gitea client whose metadata PR URL points at `pr_host` but whose
/// configured trusted origin is `trusted`. Used to drive hostile-metadata cases.
fn gitea_with_trust(pr_host: &str, trusted: &str) -> Gitea {
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    let pr_api = format!("{pr_host}/api/v1/repos/owner/repo/pulls/1");
    Gitea::with_client(
        http,
        "s3cr3t-token".to_string(),
        USER.to_string(),
        &pr_api,
        trusted,
        None,
    )
    .expect("gitea client builds")
}

/// When the metadata PR origin matches the configured trusted origin, the token
/// *is* sent (the request reaches the mock and the auth header is present).
#[tokio::test]
async fn token_sent_to_trusted_origin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(COMMENTS_PATH))
        .and(header("Authorization", "token s3cr3t-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let client = gitea_with_trust(&server.uri(), &server.uri());
    assert_eq!(client.assignee().await.unwrap(), None);
    // `expect(1)` on drop verifies exactly one authorized request was made.
}

/// A metadata PR URL on a *different host* than the trusted origin is refused
/// before any request is sent: the mock records zero hits and the error is
/// `UntrustedOrigin`, never leaking the token.
#[tokio::test]
async fn token_refused_for_foreign_host() {
    let server = MockServer::start().await;
    // Any request at all would be a leak; assert zero.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;

    // Trust a different (loopback) origin than the PR host.
    let other = "http://127.0.0.1:1";
    let client = gitea_with_trust(&server.uri(), other);
    let err = client.assignee().await.unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("refusing to send Gitea token"),
        "unexpected error: {msg}"
    );
    assert!(!msg.contains("s3cr3t-token"), "error leaked token: {msg}");
    assert!(
        !format!("{err:?}").contains("s3cr3t-token"),
        "debug leaked token"
    );
}

/// The diff fetch shares the origin guard: a foreign PR host gets no request.
#[tokio::test]
async fn pr_diff_refused_for_foreign_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x"))
        .expect(0)
        .mount(&server)
        .await;

    let client = gitea_with_trust(&server.uri(), "http://127.0.0.1:1");
    assert!(matches!(
        client.pr_diff().await,
        Err(mtui_datasources::error::GiteaError::UntrustedOrigin(_))
    ));
}

/// A same-host but *different-port* PR URL is refused (origin is exact).
#[tokio::test]
async fn token_refused_for_foreign_port() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;

    // Trust the same host on a port that is guaranteed not to be the mock's.
    let trusted = server.uri().replace(
        server.address().port().to_string().as_str(),
        // pick a different port string; port 1 is never the ephemeral mock port.
        "1",
    );
    let client = gitea_with_trust(&server.uri(), &trusted);
    assert!(matches!(
        client.assignee().await,
        Err(mtui_datasources::error::GiteaError::UntrustedOrigin(_))
    ));
}

/// An empty or unparseable `gitea_url` cannot build a client at all — there is
/// no trust anchor, so the token can never be sent anywhere.
#[test]
fn empty_or_bad_trusted_url_refuses_to_build() {
    let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
    for bad in [
        "",
        "not a url",
        "http://example.com", /* non-loopback http */
    ] {
        let r = Gitea::with_client(
            http.clone(),
            "tok".to_string(),
            USER.to_string(),
            "https://src.suse.de/api/v1/repos/o/r/pulls/1",
            bad,
            None,
        );
        assert!(r.is_err(), "should refuse to build with gitea_url={bad:?}");
    }
}

// --- pull_state: the read-only gate/holder read ---

const REVIEWS_PATH: &str = "/api/v1/repos/owner/repo/pulls/1/reviews";

fn review(id: i64, login: &str, state: &str, commit: &str) -> serde_json::Value {
    json!({
        "id": id, "state": state, "dismissed": false, "stale": false,
        "commit_id": commit, "submitted_at": ts(id as u32), "user": { "login": login }
    })
}

fn pr_json(head: &str, state: &str) -> serde_json::Value {
    json!({
        "state": state, "created_at": ts(1), "head": { "sha": head },
        "base": { "ref": "main" }, "requested_reviewers": []
    })
}

/// Mount the PR, one page of `reviews` and the empty page that ends them.
async fn mount_pull_json(server: &MockServer, pr: serde_json::Value, reviews: serde_json::Value) {
    use wiremock::matchers::query_param;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(REVIEWS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reviews))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(REVIEWS_PATH))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
}

async fn mount_pull(server: &MockServer, head: &str, state: &str, reviews: serde_json::Value) {
    mount_pull_json(server, pr_json(head, state), reviews).await;
}

/// The gate of a PR at head `abc` (open) with `reviews` and no comments.
async fn gate_for(reviews: serde_json::Value) -> PullReviewGate {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pull(&server, "abc", "open", reviews).await;
    gitea_for(&server).pull_state().await.unwrap().gate
}

fn known(verdict: PullVerdict, stale: bool, closed: bool) -> impl Fn(&PullReviewGate) -> bool {
    move |g| matches!(g, PullReviewGate::Known(k) if k.verdict == verdict && k.stale == stale && k.closed == closed)
}

fn unknown_because(gate: &PullReviewGate, why: &str) -> bool {
    matches!(gate, PullReviewGate::Unknown(r) if r == why)
}

use mtui_datasources::{PullReviewGate, PullVerdict};

#[tokio::test]
async fn pull_state_reads_the_holder_head_base_and_an_owed_review() {
    let server = MockServer::start().await;
    mount_comments(
        &server,
        json!([comment_json(1, &assign_marker("alice", GROUP), 2)]),
    )
    .await;
    mount_pull(
        &server,
        "abc",
        "open",
        json!([review(3, "qam-sle-review", "REQUEST_REVIEW", "")]),
    )
    .await;
    let state = gitea_for(&server).pull_state().await.unwrap();
    assert_eq!(state.owner.as_deref(), Some("alice"));
    assert_eq!(state.head, "abc");
    assert_eq!(state.base.as_deref(), Some("main"));
    assert_eq!(state.created.as_deref(), Some(ts(1).as_str()));
    let PullReviewGate::Known(gate) = &state.gate else {
        panic!("{:?}", state.gate);
    };
    assert_eq!(
        (gate.verdict, gate.stale, gate.closed),
        (PullVerdict::Owed, false, false)
    );
    assert_eq!(gate.since.as_deref(), Some(ts(3).as_str()));
    let methods: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.method)
        .collect();
    assert!(
        methods.iter().all(|m| *m == wiremock::http::Method::GET),
        "{methods:?}"
    );
}

/// The latest verdict wins; a dismissed one and another account's are
/// skipped, and an approval on an older commit is stale.
#[tokio::test]
async fn pull_state_takes_the_latest_live_group_verdict() {
    let mut dismissed = review(9, "qam-sle-review", "REQUEST_CHANGES", "abc");
    dismissed["dismissed"] = json!(true);
    let gate = gate_for(json!([
        review(3, "qam-sle-review", "REQUEST_REVIEW", ""),
        review(5, "qam-sle-review", "APPROVED", "old"),
        dismissed,
        review(11, "someone", "REQUEST_CHANGES", "abc"),
    ]))
    .await;
    assert!(known(PullVerdict::Approved, true, false)(&gate), "{gate:?}");
}

#[tokio::test]
async fn a_stale_flag_marks_an_approval_on_the_head_stale() {
    let mut approved = review(5, "qam-sle-review", "APPROVED", "abc");
    approved["stale"] = json!(true);
    let gate = gate_for(json!([approved])).await;
    assert!(known(PullVerdict::Approved, true, false)(&gate), "{gate:?}");
    let fresh = gate_for(json!([review(5, "qam-sle-review", "APPROVED", "abc")])).await;
    assert!(
        known(PullVerdict::Approved, false, false)(&fresh),
        "{fresh:?}"
    );
}

/// A draft beside the request, or a comment after a verdict, does not
/// decide; a comment alone (Gitea clears the request when any review is
/// submitted) leaves the group's verdict owed.
#[tokio::test]
async fn comments_and_drafts_do_not_decide() {
    let gate = gate_for(json!([
        review(3, "qam-sle-review", "REQUEST_REVIEW", ""),
        review(5, "qam-sle-review", "PENDING", "abc"),
    ]))
    .await;
    assert!(known(PullVerdict::Owed, false, false)(&gate), "{gate:?}");
    let after_verdict = gate_for(json!([
        review(3, "qam-sle-review", "APPROVED", "abc"),
        review(4, "qam-sle-review", "COMMENT", "abc"),
    ]))
    .await;
    assert!(
        known(PullVerdict::Approved, false, false)(&after_verdict),
        "{after_verdict:?}"
    );
    let re_requested = gate_for(json!([
        review(3, "qam-sle-review", "COMMENT", "abc"),
        review(4, "qam-sle-review", "REQUEST_REVIEW", ""),
    ]))
    .await;
    let PullReviewGate::Known(gate) = &re_requested else {
        panic!("{re_requested:?}");
    };
    assert_eq!(gate.verdict, PullVerdict::Owed);
    assert_eq!(gate.since.as_deref(), Some(ts(4).as_str()));
    let only_comment = gate_for(json!([review(4, "qam-sle-review", "COMMENT", "abc")])).await;
    let PullReviewGate::Known(gate) = &only_comment else {
        panic!("{only_comment:?}");
    };
    assert_eq!(
        (gate.verdict, gate.stale, gate.closed),
        (PullVerdict::Owed, false, false)
    );
    assert_eq!(gate.since.as_deref(), Some(ts(4).as_str()));
}

/// Closed only marks an owed review: a verdict on a closed pull request
/// stands as it is.
#[tokio::test]
async fn a_verdict_on_a_closed_pull_request_is_not_closed() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pull(
        &server,
        "abc",
        "closed",
        json!([review(4, "qam-sle-review", "APPROVED", "abc")]),
    )
    .await;
    let gate = gitea_for(&server).pull_state().await.unwrap().gate;
    assert!(
        known(PullVerdict::Approved, false, false)(&gate),
        "{gate:?}"
    );
}

/// A team review request carries no user: it cannot be the group's (a user
/// account), so it neither decides nor hides the group's verdict.
#[tokio::test]
async fn a_team_review_request_does_not_hide_the_group_verdict() {
    let mut team = review(12, "x", "REQUEST_REVIEW", "");
    team["user"] = json!(null);
    team["team"] = json!({"id": 5, "name": "maintainers"});
    let gate = gate_for(json!([
        team,
        review(3, "qam-sle-review", "APPROVED", "abc"),
    ]))
    .await;
    assert!(
        known(PullVerdict::Approved, false, false)(&gate),
        "{gate:?}"
    );
}

#[tokio::test]
async fn pull_state_covers_changes_requested_closed_and_no_review() {
    let changes = gate_for(json!([review(
        4,
        "qam-sle-review",
        "REQUEST_CHANGES",
        "abc"
    )]))
    .await;
    assert!(
        known(PullVerdict::ChangesRequested, false, false)(&changes),
        "{changes:?}"
    );

    let closed = MockServer::start().await;
    mount_comments(&closed, json!([])).await;
    mount_pull(
        &closed,
        "abc",
        "closed",
        json!([review(4, "qam-sle-review", "REQUEST_REVIEW", "")]),
    )
    .await;
    let gate = gitea_for(&closed).pull_state().await.unwrap().gate;
    assert!(known(PullVerdict::Owed, false, true)(&gate), "{gate:?}");

    assert_eq!(gate_for(json!([])).await, PullReviewGate::None);
}

#[tokio::test]
async fn an_unknown_review_state_is_unknown_not_a_verdict() {
    let gate = gate_for(json!([
        review(3, "qam-sle-review", "APPROVED", "abc"),
        review(4, "qam-sle-review", "SOMETHING_NEW", "abc"),
    ]))
    .await;
    assert!(
        unknown_because(&gate, "review 4 has the unknown state \"SOMETHING_NEW\""),
        "{gate:?}"
    );
}

#[tokio::test]
async fn an_absent_dismissed_or_stale_field_is_unknown() {
    let mut no_dismissed = review(5, "qam-sle-review", "APPROVED", "abc");
    no_dismissed.as_object_mut().unwrap().remove("dismissed");
    let gate = gate_for(json!([no_dismissed])).await;
    assert!(
        unknown_because(&gate, "review 5 has no `dismissed` field"),
        "{gate:?}"
    );
    let mut no_stale = review(5, "qam-sle-review", "APPROVED", "abc");
    no_stale.as_object_mut().unwrap().remove("stale");
    let gate = gate_for(json!([no_stale])).await;
    assert!(
        unknown_because(&gate, "review 5 has no `stale` field"),
        "{gate:?}"
    );
}

/// A review with neither a user nor a team may be the group's, so the gate
/// is unknown, never "no review".
#[tokio::test]
async fn a_review_without_a_readable_reviewer_is_unknown() {
    let mut anonymous = review(4, "x", "APPROVED", "abc");
    anonymous["user"] = json!(null);
    anonymous["team"] = json!(null);
    let gate = gate_for(json!([anonymous])).await;
    assert!(
        unknown_because(&gate, "review 4 carries no readable reviewer"),
        "{gate:?}"
    );
}

#[tokio::test]
async fn an_owed_review_on_a_pr_without_state_is_unknown() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pull_json(
        &server,
        json!({"head": {"sha": "abc"}}),
        json!([review(4, "qam-sle-review", "REQUEST_REVIEW", "")]),
    )
    .await;
    let state = gitea_for(&server).pull_state().await.unwrap();
    assert!(
        unknown_because(&state.gate, "the pull request has no open or closed state"),
        "{:?}",
        state.gate
    );
    assert_eq!((state.created, state.base), (None, None));
}

/// A state other than open/closed is not read as open.
#[tokio::test]
async fn an_owed_review_on_a_pr_in_an_unknown_state_is_unknown() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    mount_pull(
        &server,
        "abc",
        "merged",
        json!([review(4, "qam-sle-review", "REQUEST_REVIEW", "")]),
    )
    .await;
    let gate = gitea_for(&server).pull_state().await.unwrap().gate;
    assert!(
        unknown_because(&gate, "the pull request has no open or closed state"),
        "{gate:?}"
    );
}

#[tokio::test]
async fn reviews_past_the_page_bound_are_a_failure() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_json("abc", "open")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(REVIEWS_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([review(1, "someone", "COMMENT", "abc")])),
        )
        .mount(&server)
        .await;
    let err = gitea_for(&server).pull_state().await.unwrap_err();
    assert!(
        matches!(&err, GiteaError::FailedCall(m) if m.starts_with("more than 20 pages of reviews")),
        "{err:?}"
    );
    let pages = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == REVIEWS_PATH)
        .count();
    assert_eq!(pages, 20);
}

#[tokio::test]
async fn pull_state_fails_when_a_read_fails() {
    let server = MockServer::start().await;
    mount_comments(&server, json!([])).await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"head": {"sha": "abc"}})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(REVIEWS_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = gitea_for(&server).pull_state().await.unwrap_err();
    assert!(
        matches!(&err, GiteaError::FailedCall(m) if m.contains("/reviews") && m.contains("503")),
        "{err:?}"
    );
}
