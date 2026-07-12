//! P7.1 spike gate: in-process MCP round-trip.
//!
//! The Rust analogue of upstream `test_mcp_stdio_roundtrip.py`. It connects an
//! rmcp client to the spike [`SpikeServer`] over an in-memory duplex transport
//! (no subprocess, no socket) and proves the runtime-registration wiring:
//!
//! 1. `tools/list` reflects the auto-generated `whoami` tool synthesised from
//!    the command registry.
//! 2. `call_tool("whoami")` routes through the *same* engine the REPL uses and
//!    returns the `User: <user>, app pid: …` banner the command prints.
//!
//! This is the walking skeleton that de-risks Phase 7: it demonstrates a
//! hand-written `ServerHandler` with a runtime-built tool set + schema works
//! against rmcp 2.x, settling the "runtime handler vs `#[tool]` macro vs raw
//! JSON-RPC" question in favour of the runtime handler.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::register_all;
use mtui_mcp::provider::{SessionProvider, StdioProvider};
use mtui_mcp::server::SpikeServer;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

/// Builds a spike server over a session whose user is a known fixed value,
/// resolved through the stdio provider (the transport-agnostic seam).
async fn build_server() -> SpikeServer {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    let registry = Arc::new(register_all());
    let provider = StdioProvider::new(config);
    let session = provider.get_or_create("<default>").await;
    SpikeServer::new(registry, session)
}

#[tokio::test]
async fn roundtrip_lists_and_calls_whoami() {
    // In-memory bidirectional transport: `serve` consumes an (AsyncRead,
    // AsyncWrite); a single duplex gives two ends that talk to each other.
    let (server_io, client_io) = tokio::io::duplex(4096);

    let server = build_server().await;
    let server_task = tokio::spawn(async move {
        // `serve` runs the initialize handshake, then the request loop until the
        // peer disconnects.
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });

    // The client side: `()` is the no-op ClientHandler; `serve` performs the
    // client half of the handshake and returns a peer to drive requests.
    let client = ().serve(client_io).await.expect("client serve/initialize");

    // (1) tools/list reflects the synthesised command tool.
    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"whoami"),
        "expected `whoami` in tools/list, got: {names:?}"
    );

    // (2) call_tool routes through the engine and returns the REPL banner.
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

    // Tear down: dropping the client closes the transport, ending the server.
    drop(client);
    let _ = server_task.await;
}
