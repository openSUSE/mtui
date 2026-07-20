//! Integration tests for the Slack review-request connector against a real
//! HTTP transport (`wiremock`).
//!
//! The cases that matter here are the ones a unit test cannot reach: Slack's
//! `ok: false`-inside-HTTP-200 error convention, `429` handling, and cursor
//! pagination. wiremock matches by request shape rather than call order, so a
//! sequence of differing responses to the same endpoint is expressed with
//! `expect`-scoped mounts or by matching on the request body.

use mtui_datasources::slack::Slack;
use mtui_datasources::{HttpClient, SlackError, VerifyPolicy};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHANNEL: &str = "C0123456789";
const TS: &str = "1700000000.000100";

/// Build a Slack client whose API base resolves to `server`.
///
/// Loopback plain HTTP is deliberately permitted by the token-safety guard so
/// this is possible; see `is_token_safe_url`.
fn slack_for(server: &MockServer) -> Slack {
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    Slack::with_client(http, "xoxb-test".to_owned(), &server.uri()).expect("slack client builds")
}

/// Mount a JSON response for `api_method`.
async fn mount(server: &MockServer, api_method: &str, body: serde_json::Value) {
    Mock::given(path(format!("/{api_method}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn post_message_returns_canonical_channel_and_ts() {
    let server = MockServer::start().await;
    // The caller posted to `#qam-review`; Slack answers with the channel ID.
    // Persisting the ID rather than the name is what makes the later
    // reactions/replies reads work at all.
    mount(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "channel": CHANNEL, "ts": TS }),
    )
    .await;

    let posted = slack_for(&server)
        .post_message("#qam-review", "Please review SUSE:Maintenance:1:2")
        .await
        .expect("post succeeds");

    assert_eq!(posted.channel, CHANNEL);
    assert_eq!(posted.ts, TS);
}

#[tokio::test]
async fn post_message_sends_bearer_token_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .and(body_string_contains("SUSE:Maintenance:1:2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "channel": CHANNEL, "ts": TS
        })))
        .mount(&server)
        .await;

    slack_for(&server)
        .post_message(CHANNEL, "Please review SUSE:Maintenance:1:2")
        .await
        .expect("post succeeds");

    let requests = server.received_requests().await.unwrap();
    let auth = requests[0]
        .headers
        .get("authorization")
        .expect("authorization header is sent")
        .to_str()
        .unwrap();
    assert_eq!(auth, "Bearer xoxb-test");
}

#[tokio::test]
async fn application_error_in_http_200_is_an_api_error() {
    let server = MockServer::start().await;
    // Slack's defining quirk: the call "succeeded" at the HTTP layer and still
    // failed. Trusting the status here would report a silent non-delivery as a
    // posted review request.
    mount(
        &server,
        "chat.postMessage",
        json!({ "ok": false, "error": "channel_not_found" }),
    )
    .await;

    let err = slack_for(&server)
        .post_message("#nope", "hi")
        .await
        .unwrap_err();

    match err {
        SlackError::Api(code) => assert_eq!(code, "channel_not_found"),
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_error_field_still_reports_an_api_error() {
    let server = MockServer::start().await;
    // A malformed `ok: false` without an error code must not be read as success.
    mount(&server, "chat.postMessage", json!({ "ok": false })).await;

    match slack_for(&server).post_message(CHANNEL, "hi").await {
        Err(SlackError::Api(code)) => assert_eq!(code, "unknown"),
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_is_typed_and_carries_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/reactions.get"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;

    let err = slack_for(&server).reactions(CHANNEL, TS).await.unwrap_err();

    match err {
        SlackError::RateLimited { retry_after } => assert_eq!(retry_after, Some(30)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_without_a_header_is_still_typed() {
    let server = MockServer::start().await;
    // The variant must survive a missing or unparseable header, since the
    // watch loop's back-off decision depends on the *type*, not the number.
    Mock::given(path("/reactions.get"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "soon"))
        .mount(&server)
        .await;

    match slack_for(&server).reactions(CHANNEL, TS).await {
        Err(SlackError::RateLimited { retry_after }) => assert_eq!(retry_after, None),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn reactions_are_skin_tone_normalised() {
    let server = MockServer::start().await;
    mount(
        &server,
        "reactions.get",
        json!({
            "ok": true,
            "message": { "reactions": [
                { "name": "+1::skin-tone-5", "users": ["U1", "U2"] },
                { "name": "eyes", "users": ["U3"] }
            ]}
        }),
    )
    .await;

    let reactions = slack_for(&server)
        .reactions(CHANNEL, TS)
        .await
        .expect("reactions read");

    assert_eq!(reactions.len(), 2);
    // The tone modifier is gone, so the command can compare against "+1".
    assert_eq!(reactions[0].name, "+1");
    assert_eq!(reactions[0].users, vec!["U1", "U2"]);
    assert_eq!(reactions[1].name, "eyes");
}

#[tokio::test]
async fn reactions_query_identifies_the_message() {
    let server = MockServer::start().await;
    Mock::given(path("/reactions.get"))
        .and(query_param("channel", CHANNEL))
        .and(query_param("timestamp", TS))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "message": { "reactions": [] }
        })))
        .mount(&server)
        .await;

    // Mounting on the exact query means a wrong parameter name would 404 here
    // rather than silently watching the wrong message.
    let reactions = slack_for(&server)
        .reactions(CHANNEL, TS)
        .await
        .expect("reactions read");
    assert!(reactions.is_empty());
}

#[tokio::test]
async fn no_reactions_field_is_an_empty_list_not_an_error() {
    let server = MockServer::start().await;
    // Nobody has reacted yet — the overwhelmingly common case during a watch.
    mount(
        &server,
        "reactions.get",
        json!({ "ok": true, "message": { "text": "Please review" } }),
    )
    .await;

    let reactions = slack_for(&server)
        .reactions(CHANNEL, TS)
        .await
        .expect("absent reactions is not a failure");
    assert!(reactions.is_empty());
}

#[tokio::test]
async fn replies_excludes_the_parent_message() {
    let server = MockServer::start().await;
    mount(
        &server,
        "conversations.replies",
        json!({
            "ok": true,
            "messages": [
                { "ts": TS, "user": "UBOT", "text": "Please review SUSE:Maintenance:1:2" },
                { "ts": "1700000001.000200", "user": "U1", "text": "looks good" }
            ]
        }),
    )
    .await;

    let replies = slack_for(&server)
        .replies(CHANNEL, TS)
        .await
        .expect("replies read");

    // The parent is the request itself; counting it as a reply would make an
    // unanswered request look answered.
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].user, "U1");
    assert_eq!(replies[0].text, "looks good");
}

#[tokio::test]
async fn replies_follow_the_cursor() {
    let server = MockServer::start().await;
    // Page 2 is mounted first: wiremock matches most-recently-mounted first,
    // so the cursor-bearing request is answered by this narrower mount and the
    // cursorless one falls through to the page-1 mount below.
    Mock::given(path("/conversations.replies"))
        .and(query_param("cursor", "page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "messages": [{ "ts": "1700000002.000300", "user": "U2", "text": "second" }]
        })))
        .mount(&server)
        .await;
    Mock::given(path("/conversations.replies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "messages": [{ "ts": "1700000001.000200", "user": "U1", "text": "first" }],
            "response_metadata": { "next_cursor": "page2" }
        })))
        .mount(&server)
        .await;

    let replies = slack_for(&server)
        .replies(CHANNEL, TS)
        .await
        .expect("replies read");

    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].text, "first");
    assert_eq!(replies[1].text, "second");
}

#[tokio::test]
async fn replies_stop_at_the_page_cap() {
    let server = MockServer::start().await;
    // A cursor that never clears would otherwise spin forever inside a single
    // poll of the watch loop.
    Mock::given(path("/conversations.replies"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "messages": [{ "ts": "1700000001.000200", "user": "U1", "text": "loop" }],
            "response_metadata": { "next_cursor": "always-more" }
        })))
        .mount(&server)
        .await;

    let replies = slack_for(&server)
        .replies(CHANNEL, TS)
        .await
        .expect("capped read still succeeds");

    // MAX_REPLY_PAGES pages, one reply each.
    assert_eq!(replies.len(), 10);
    assert_eq!(server.received_requests().await.unwrap().len(), 10);
}

#[tokio::test]
async fn auth_test_returns_the_bot_user_id() {
    let server = MockServer::start().await;
    mount(
        &server,
        "auth.test",
        json!({ "ok": true, "user_id": "UBOT123", "team": "T1" }),
    )
    .await;

    let id = slack_for(&server).auth_test().await.expect("auth ok");
    assert_eq!(id, "UBOT123");
}

#[tokio::test]
async fn invalid_auth_surfaces_slacks_own_code() {
    let server = MockServer::start().await;
    mount(
        &server,
        "auth.test",
        json!({ "ok": false, "error": "invalid_auth" }),
    )
    .await;

    match slack_for(&server).auth_test().await {
        Err(SlackError::Api(code)) => assert_eq!(code, "invalid_auth"),
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn server_error_status_is_a_failed_call() {
    let server = MockServer::start().await;
    Mock::given(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    match slack_for(&server).post_message(CHANNEL, "hi").await {
        Err(SlackError::FailedCall(msg)) => {
            assert!(msg.contains("503"), "names the status: {msg}");
            assert!(!msg.contains("xoxb"), "must not leak the token: {msg}");
        }
        other => panic!("expected FailedCall, got {other:?}"),
    }
}

#[tokio::test]
async fn non_json_body_is_a_failed_call_not_a_panic() {
    let server = MockServer::start().await;
    // A proxy or captive portal answering with HTML must not take mtui down.
    Mock::given(path("/auth.test"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>nope</html>"))
        .mount(&server)
        .await;

    assert!(matches!(
        slack_for(&server).auth_test().await,
        Err(SlackError::FailedCall(_))
    ));
}
