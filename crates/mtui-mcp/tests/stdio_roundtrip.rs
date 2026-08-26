//! The MCP round-trip contract test.
//!
//! It connects an rmcp client to the production [`McpServer`] over an in-memory
//! duplex transport (no subprocess, no socket) and proves the runtime-synthesis
//! wiring end to end:
//!
//! 1. `tools/list` reflects the full synthesised surface (command tools + job
//!    tools + the hand-written testreport tools) and **omits** every deny-listed
//!    command.
//! 2. `call_tool("whoami")` routes through the *same* engine the REPL uses and
//!    returns the `User: <user>, app pid: …` banner the command prints.
//! 3. A deny-listed tool call is rejected (`method_not_found`) — no route exists.
//!
//! This is the gating contract test: it demonstrates the hand-written
//! `ServerHandler` with a runtime-built tool set + schemas works against rmcp 3.x
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
        // Deny-listed commands must never surface, nor the removed `lrun`, nor
        // the bare `config` (it is fanned out into config_show/config_set).
        for denied in [
            "quit", "exit", "EOF", "edit", "shell", "lrun", "help", "terms", "switch", "config",
        ] {
            assert!(
                !names.contains(&denied),
                "denied/omitted `{denied}` leaked into tools/list: {names:?}"
            );
        }

        // The hand-written testreport tools.
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

/// #434: the hand-written in-band transfer tools are wired into the RUNNING
/// server — advertised on the wire and dispatched (not just present as
/// descriptors). Dropping the `.chain(transfer_descriptors)` in `build` or the
/// transfer dispatch arm in `call_tool` fails this.
#[tokio::test]
async fn transfer_tools_are_served_and_dispatched() {
    with_client(|client| async move {
        let tools = client.list_all_tools().await.expect("list tools");
        for name in ["get", "put"] {
            let tool = tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .unwrap_or_else(|| panic!("`{name}` missing from tools/list"));
            // The in-band replacements, not the synthesized path-based forms.
            assert!(
                tool.description
                    .as_deref()
                    .unwrap_or("")
                    .contains("in-band")
                    || tool
                        .description
                        .as_deref()
                        .unwrap_or("")
                        .contains("payload carried in the call"),
                "`{name}` must be the in-band tool: {:?}",
                tool.description
            );
        }

        // Dispatch reaches the transfer arm: with no report loaded the tool
        // refuses with its own message (method_not_found would mean the
        // dispatch arm is missing; a path in the output would mean the old
        // synthesized command answered).
        let out = client
            .call_tool(
                CallToolRequestParams::new("get").with_arguments(
                    serde_json::json!({ "remote": "/etc/os-release" })
                        .as_object()
                        .cloned()
                        .expect("object"),
                ),
            )
            .await
            .expect("call returns a tool result, not a protocol error");
        assert_eq!(out.is_error, Some(true));
        let text = format!("{:?}", out.content);
        assert!(
            text.contains("no report loaded"),
            "the in-band tool answered: {text}"
        );
    })
    .await;
}

/// #434: `[mcp] tools_deny` removes a transfer tool from the wire surface AND
/// from dispatch — the retain-sets stay in lockstep with the advertised list.
#[tokio::test]
async fn tools_deny_removes_transfer_tool_from_surface_and_dispatch() {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    config.mcp_tools_deny = vec!["put".to_owned()];
    let registry = Arc::new(register_all());
    let provider = StdioProvider::new(config);
    let session = provider.get_or_create("<default>").await;
    let server = McpServer::new(registry, session);

    let (server_io, client_io) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });
    let client = ().serve(client_io).await.expect("client serve");

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"get"), "get stays: {names:?}");
    assert!(!names.contains(&"put"), "put denied: {names:?}");

    let denied = client
        .call_tool(
            CallToolRequestParams::new("put").with_arguments(
                serde_json::json!({ "filename": "f", "content": "x" })
                    .as_object()
                    .cloned()
                    .expect("object"),
            ),
        )
        .await;
    assert!(
        denied.is_err(),
        "a denied tool must not dispatch: {denied:?}"
    );

    drop(client);
    let _ = server_task.await;
}
