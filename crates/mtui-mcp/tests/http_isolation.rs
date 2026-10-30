//! Per-client session isolation for the http transport.
//!
//! Under `--transport http` one process serves many clients, and each must see
//! **only its own** loaded template + SSH `targets`; sharing one session would
//! let one client's `load_template` clobber another's. rmcp's streamable-HTTP
//! transport enforces that by calling [`SessionRegistry::make_server`] once per
//! new MCP session, so the offline property to prove at that factory boundary is:
//! **it mints independent sessions whose state does not bleed.**
//!
//! The 2026-07-28 refusal is pinned over the real
//! [`StreamableHttpService`](rmcp::transport::StreamableHttpService), not over
//! `negotiate_initialize`: that helper is pure (no session touch) and answers
//! 2025-11-25 even when the HTTP list wrongly gains 2026-07-28, so it pins
//! rmcp rather than mtui.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::register_all;
use mtui_mcp::SessionRegistry;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;

fn registry(user: &str) -> SessionRegistry {
    let mut config = Config::default();
    config.session_user = user.to_owned();
    SessionRegistry::new(Arc::new(register_all()), config)
}

/// Two sessions minted from the same registry are **distinct instances** — the
/// baseline of per-client isolation (no shared `Arc<McpSession>`).
#[tokio::test]
async fn factory_mints_distinct_sessions() {
    let reg = registry("alice");

    let a = reg.make_session();
    let b = reg.make_session();

    assert!(
        !Arc::ptr_eq(&a, &b),
        "each MCP session must get its own McpSession instance"
    );
}

/// Each session's capture sink is independent: output produced on one session is
/// never observable through another's sink.
#[tokio::test]
async fn sessions_capture_output_independently() {
    let reg = registry("carol");
    let cmds = register_all();

    let a = reg.make_session();
    let b = reg.make_session();

    let out_a = a
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on a");
    assert!(
        out_a.contains("User: carol"),
        "a produced output: {out_a:?}"
    );

    // b never ran a command, so its sink is empty — a's output did not leak.
    assert_eq!(
        b.output().take(),
        "",
        "b's sink must be untouched by a's run"
    );
}

/// Sessions minted from different registries carry their **own** config clone:
/// each `whoami` reflects that registry's `session_user`, proving `make_session`
/// isolates per-session config rather than sharing one instance.
#[tokio::test]
async fn each_registry_isolates_its_config() {
    let cmds = register_all();

    let dave = registry("dave").make_session();
    let erin = registry("erin").make_session();

    let out_dave = dave
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on dave");
    let out_erin = erin
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on erin");

    assert!(out_dave.contains("User: dave"), "got: {out_dave:?}");
    assert!(out_erin.contains("User: erin"), "got: {out_erin:?}");
}

const ISOLATION_RRID: &str = "SUSE:Maintenance:1:1";

/// Two http sessions keep the session model: a template loaded in one is
/// invisible in the other.
///
/// Distinct from `factory_mints_distinct_sessions` (instance identity) and
/// `sessions_capture_output_independently` (capture sink): only this test loads
/// a template and checks the sibling stays empty. No `negotiate_initialize`
/// call: that helper is pure and never touches the session, so negotiating
/// first proves nothing about isolation.
#[tokio::test]
async fn sessions_keep_template_isolation() {
    let reg = registry("alice");
    let _a_server = reg.try_make_server().expect("first http server");
    let _b_server = reg.try_make_server().expect("second http server");
    // Keep the servers alive: dropping one frees its registry slot and its
    // session, so the live set below must be read while both are held.
    let sessions = reg.live_sessions();
    assert_eq!(sessions.len(), 2, "two clients, two sessions");

    let mut guard = sessions[0].session().lock().await;
    let mut report = ObsReport::new(guard.config.clone());
    report.base_mut().rrid = Some(RequestReviewID::parse(ISOLATION_RRID).expect("rrid"));
    guard.templates.add(Box::new(report));
    guard.templates.set_active(ISOLATION_RRID);
    assert!(
        guard.templates.contains(ISOLATION_RRID),
        "fixture must arm the assertion — the template is loaded in one session"
    );
    drop(guard);

    let other = sessions[1].session().lock().await;
    assert!(
        !other.templates.contains(ISOLATION_RRID),
        "the other session must not see the loaded template"
    );
    assert!(
        other.templates.is_empty(),
        "the other session must stay empty"
    );
}

/// Jobs resolve across calls within one session but never across sessions:
/// `job_status` on the owning session sees the job, the sibling session
/// reports it unknown, and `job_cancel` on the sibling refuses while the
/// owner's cancel succeeds.
///
/// Distinct from the `mcp_jobs` suite (single-session lifecycle: start, poll,
/// cancel, caps): only this test starts a job in one factory-minted session
/// and probes it from the sibling. No `negotiate_initialize` call, for the
/// same reason as above.
#[tokio::test]
async fn sessions_keep_job_isolation() {
    use std::time::Duration;

    use mtui_mcp::{JobState, McpSession};

    async fn await_terminal(session: &McpSession, job_id: &str) -> JobState {
        for _ in 0..500 {
            let state = session.job_status(job_id).expect("job exists").state;
            if state != JobState::Running {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("job {job_id} did not reach a terminal state");
    }

    let reg = registry("alice");
    let _a_server = reg.try_make_server().expect("first http server");
    let _b_server = reg.try_make_server().expect("second http server");
    let sessions = reg.live_sessions();
    assert_eq!(sessions.len(), 2, "two clients, two sessions");
    let (a, b) = (Arc::clone(&sessions[0]), Arc::clone(&sessions[1]));

    let cmds = Arc::new(register_all());
    let job_id = a
        .start_job(Arc::clone(&cmds), "whoami", Vec::new())
        .expect("start_job succeeds");
    assert_eq!(await_terminal(&a, &job_id).await, JobState::Done);
    // Same session, later call: the job resolves.
    assert!(
        a.job_status(&job_id).is_ok(),
        "job_status must resolve across calls in the same session"
    );
    // Sibling session: unknown.
    let err = b
        .job_status(&job_id)
        .expect_err("sibling must not see the job");
    assert!(err.stderr.contains("no such job"), "got: {err:?}");
    let err = b
        .job_cancel(&job_id)
        .await
        .expect_err("sibling must not cancel the job");
    assert!(err.stderr.contains("no such job"), "got: {err:?}");
    // Owning session: `job_cancel` resolves too (truthful no-op on a done job).
    let msg = a
        .job_cancel(&job_id)
        .await
        .expect("owner's cancel resolves");
    assert!(msg.contains("already done"), "got: {msg:?}");
}

/// The http posture over the real transport: a stateless 2026-07-28 request is
/// refused with `-32022` and mints no session, while a legacy `initialize`
/// still mints one.
///
/// Spins the production shape ([`mtui_mcp::http_app`] — the same
/// `StreamableHttpService` + body layer `serve_http` mounts) on a loopback
/// socket. Widening `SUPPORTED_PROTOCOL_VERSIONS_HTTP` with 2026-07-28 turns
/// the refused call into a dispatched one (no `-32022`); flipping
/// `legacy_session_mode` off stops the legacy handshake minting a session —
/// either reddens this test.
#[tokio::test]
async fn http_stateless_2026_07_28_is_refused_without_session() {
    use std::time::Duration;

    let reg = registry("alice");
    assert_eq!(reg.live_count(), 0, "no session before any request");

    // The production knobs `http_app` takes; `registry()` only overrides
    // `session_user`, so the mcp defaults match the registry's own.
    let defaults = Config::default();
    let keep_alive = (defaults.mcp_session_idle_timeout != 0)
        .then(|| Duration::from_secs(defaults.mcp_session_idle_timeout));
    let app = mtui_mcp::http_app(reg.clone(), keep_alive, defaults.mcp_max_request_bytes);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let url = format!("http://{addr}/");
    let client = reqwest::Client::new();

    async fn post_stateless_2026_07_28(client: &reqwest::Client, url: &str) -> reqwest::Response {
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "whoami")
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "whoami",
                        "arguments": {},
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .expect("stateless POST reaches the transport")
    }

    for round in 1..=2 {
        let response = post_stateless_2026_07_28(&client, &url).await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "round {round}: stateless 2026-07-28 must map to HTTP 400"
        );
        assert!(
            response.headers().get("mcp-session-id").is_none(),
            "round {round}: a refused stateless request mints no session id"
        );
        let body: serde_json::Value = response.json().await.expect("JSON-RPC body");
        assert_eq!(
            body["error"]["code"], -32022,
            "round {round}: 2026-07-28 tools/call must be refused, got: {body}"
        );
        assert_eq!(
            reg.live_count(),
            0,
            "round {round}: no session may linger after a refused stateless request"
        );
    }

    // Control: the legacy handshake still mints a session, pinning
    // `legacy_session_mode(true)`.
    let init = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "http-isolation", "version": "0"},
                },
            })
            .to_string(),
        )
        .send()
        .await
        .expect("legacy initialize reaches the transport");
    assert!(
        init.status().is_success(),
        "legacy initialize must succeed, got HTTP {}",
        init.status()
    );
    assert!(
        init.headers().get("mcp-session-id").is_some(),
        "legacy initialize must mint a session id"
    );
    assert_eq!(
        reg.live_count(),
        1,
        "one legacy session must be tracked after initialize"
    );
    let text = init.text().await.expect("initialize SSE body");
    assert!(
        text.contains("2025-11-25"),
        "legacy handshake must negotiate 2025-11-25, got: {text}"
    );

    server.abort();
}

/// The disabled body-limit path composes: `max_request_bytes == 0` builds the
/// same production app with axum's limit disabled (and rmcp's own cap lifted),
/// pinning the `None` arm `http_app` carries for that knob.
#[test]
fn http_app_builds_with_disabled_body_limit() {
    use std::time::Duration;

    let defaults = Config::default();
    let keep_alive = (defaults.mcp_session_idle_timeout != 0)
        .then(|| Duration::from_secs(defaults.mcp_session_idle_timeout));
    let _app = mtui_mcp::http_app(registry("alice"), keep_alive, 0);
}
