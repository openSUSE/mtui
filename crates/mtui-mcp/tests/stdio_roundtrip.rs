//! P7.7 gate: the MCP round-trip contract test.
//!
//! The Rust analogue of upstream `test_mcp_stdio_roundtrip.py`. It connects an
//! rmcp client to the production [`McpServer`] over an in-memory duplex transport
//! (no subprocess, no socket) and proves the runtime-synthesis wiring end to end:
//!
//! 1. `tools/list` reflects the full synthesised surface (command tools + job
//!    tools + the hand-written testreport tools) and **omits** every deny-listed
//!    REPL-only command.
//! 2. `call_tool("whoami")` routes through the *same* engine the REPL uses and
//!    returns the `User: <user>, app pid: …` banner the command prints.
//! 3. A deny-listed tool call is rejected (`method_not_found`) — no route exists.
//!
//! This is the Phase-7 gating contract test: it demonstrates the hand-written
//! `ServerHandler` with a runtime-built tool set + schemas works against rmcp 2.x
//! over a transport.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::register_all;
use mtui_mcp::provider::{SessionProvider, StdioProvider};
use mtui_mcp::server::McpServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

/// Builds the production server over a session whose user is a known fixed value,
/// resolved through the stdio provider (the transport-agnostic seam).
async fn build_server() -> McpServer {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    let registry = Arc::new(register_all());
    let provider = StdioProvider::new(config);
    let session = provider.get_or_create("<default>").await;
    McpServer::new(registry, session)
}

/// Connect an in-memory rmcp client to a freshly-built server and hand the peer
/// to `body`. The server task ends when the client is dropped.
async fn with_client<F, Fut, T>(body: F) -> T
where
    F: FnOnce(rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    // In-memory bidirectional transport: a single duplex gives two ends that
    // talk to each other (no subprocess, no socket).
    let (server_io, client_io) = tokio::io::duplex(4096);

    let server = build_server().await;
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });

    // `()` is the no-op ClientHandler; `serve` performs the client half of the
    // handshake and returns a peer to drive requests.
    let client = ().serve(client_io).await.expect("client serve/initialize");
    let out = body(client).await;
    let _ = server_task.await;
    out
}

#[tokio::test]
async fn tools_list_reflects_synthesised_surface_and_denylist() {
    with_client(|client| async move {
        let tools = client.list_all_tools().await.expect("list tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();

        // Command tools synthesised from the registry.
        for expected in ["whoami", "run", "config_show", "config_set"] {
            assert!(
                names.contains(&expected),
                "expected `{expected}` in tools/list, got: {names:?}"
            );
        }
        // The four background-job control tools.
        for expected in ["job_list", "job_status", "job_result", "job_cancel"] {
            assert!(
                names.contains(&expected),
                "expected job tool `{expected}` in tools/list, got: {names:?}"
            );
        }
        // Deny-listed REPL-only commands must never surface, nor the bare
        // `config` (it is fanned out into config_show/config_set).
        for denied in [
            "quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch", "config",
        ] {
            assert!(
                !names.contains(&denied),
                "denied/omitted `{denied}` leaked into tools/list: {names:?}"
            );
        }

        // The hand-written testreport tools (bead mtui-rs-76e.8).
        for expected in [
            "testreport_read",
            "testreport_logs",
            "testreport_patch",
            "testreport_write",
            "testreport_fill",
        ] {
            assert!(
                names.contains(&expected),
                "expected testreport tool `{expected}` in tools/list, got: {names:?}"
            );
        }

        // whoami carries the read-only annotation.
        let whoami = tools
            .iter()
            .find(|t| t.name.as_ref() == "whoami")
            .expect("whoami present");
        assert_eq!(
            whoami.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(true),
            "whoami should carry readOnlyHint=true"
        );
    })
    .await;
}

#[tokio::test]
async fn call_whoami_routes_through_the_engine() {
    with_client(|client| async move {
        let result = client
            .call_tool(CallToolRequestParams::new("whoami"))
            .await
            .expect("call whoami");
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or_default();
        assert!(
            text.starts_with("User: testuser, app pid: "),
            "unexpected tool output: {text:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn call_denied_tool_is_method_not_found() {
    with_client(|client| async move {
        // `shell` is deny-listed, so the server synthesised no route for it and
        // rejects the call as an unknown method.
        let err = client
            .call_tool(CallToolRequestParams::new("shell"))
            .await
            .expect_err("denied tool must be rejected");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("method")
                || msg.to_lowercase().contains("not found")
                || msg.contains("-32601"),
            "expected a method-not-found error, got: {msg}"
        );
    })
    .await;
}
