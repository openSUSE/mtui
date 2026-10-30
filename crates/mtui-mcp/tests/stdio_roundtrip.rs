//! The MCP round-trip contract test.
//!
//! Connects an rmcp client to the production [`McpServer`] over an in-memory
//! duplex transport (no subprocess, no socket) and proves the runtime-synthesis
//! wiring end to end:
//!
//! 1. `tools/list` reflects the full synthesised surface (command tools + job
//!    tools + the hand-written report-document tools) and **omits** every deny-listed
//!    command.
//! 2. `call_tool("whoami")` routes through the *same* engine the REPL uses and
//!    returns the `User: <user>, app pid: …` banner the command prints.
//! 3. A deny-listed tool call is rejected (`method_not_found`) — no route exists.
//! 4. #591: opening with `server/discover` at the 2026-07-28 draft succeeds
//!    (not `-32022`), and a follow-up `tools/call` on the same connection
//!    works too — driven over raw JSON-RPC lines, since rmcp's client-side
//!    `ServiceExt::serve` always opens with `initialize`.

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
    build_server_with_config(config).await
}

/// [`build_server`] with an explicit config, so wire tests can point
/// `[mcp] audit_log` at a temp file.
async fn build_server_with_config(config: Config) -> McpServer {
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
    // A single duplex gives two ends that talk to each other.
    let (server_io, client_io) = tokio::io::duplex(4096);

    let server = build_server().await;
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });

    // `()` is the no-op ClientHandler; `serve` does the client half of the
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
        for expected in ["whoami", "run", "config_show"] {
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
        // Deny-listed commands must never surface, nor the removed `lrun` and
        // `terms`, nor the bare `config` (fanned out; only `config_show`
        // survives — `config_set` is denied, #410).
        for denied in [
            "quit",
            "exit",
            "EOF",
            "edit",
            "shell",
            "lrun",
            "help",
            "terms",
            "switch",
            "config",
            "config_set",
        ] {
            assert!(
                !names.contains(&denied),
                "denied/omitted `{denied}` leaked into tools/list: {names:?}"
            );
        }

        // The hand-written report-document tools; the retired text tools stay gone.
        for expected in ["report_sections", "report_section_write", "report_files"] {
            assert!(
                names.contains(&expected),
                "expected report tool `{expected}` in tools/list, got: {names:?}"
            );
        }
        assert!(
            !names.iter().any(|n| n.starts_with("testreport")),
            "a testreport tool is still listed: {names:?}"
        );

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

/// #410, dispatch half: `config_set` is denied, so the call is
/// `method_not_found` (-32601) — not a command error.
#[tokio::test]
async fn call_config_set_is_method_not_found() {
    with_client(|client| async move {
        let err = client
            .call_tool(
                CallToolRequestParams::new("config_set").with_arguments(
                    serde_json::json!({"attribute": "session_user", "value": "x"})
                        .as_object()
                        .cloned()
                        .expect("object"),
                ),
            )
            .await
            .expect_err("denied tool must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("-32601"),
            "expected a method-not-found error, got: {msg}"
        );
    })
    .await;
}

/// #410, dispatch half: an openQA operator override is accepted and reaches
/// the engine — the call answers with an error payload carrying the
/// `require_update` refusal (`load_template`), not a protocol error, and
/// never an unknown-argument refusal.
///
/// Non-goal, stated so this is not mistaken for wiring coverage: with no
/// report loaded both probes fail at `require_update` before the override is
/// read, so this passes with the dispatch strip deleted too. Engine-inertness
/// is pinned by `dispatch_openqa_overrides_are_ignored_until_26_5` in
/// `crates/mtui-mcp/src/tools.rs`, which drives dispatch past
/// `require_update` against a mock server.
#[tokio::test]
async fn call_openqa_override_is_ignored() {
    with_client(|client| async move {
        let plain = client
            .call_tool(CallToolRequestParams::new("openqa_overview"))
            .await
            .expect("the call answers");
        let overridden = client
            .call_tool(
                CallToolRequestParams::new("openqa_overview").with_arguments(
                    serde_json::json!({"url_openqa": "http://evil.example/"})
                        .as_object()
                        .cloned()
                        .expect("object"),
                ),
            )
            .await
            .expect("a shimmed kwarg answers too");
        for out in [&plain, &overridden] {
            assert_eq!(out.is_error, Some(true));
            let text = format!("{:?}", out.content);
            assert!(
                text.contains("load_template"),
                "the call must reach the engine's require_update refusal: {text}"
            );
            assert!(
                !text.contains("unknown argument"),
                "the override must be ignored, not refused: {text}"
            );
        }
        assert_eq!(
            format!("{:?}", plain.content),
            format!("{:?}", overridden.content),
            "the override changes nothing"
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
            // The in-band replacements, not the synthesized path-based forms:
            // both carry the content in the call (`content_b64`).
            assert!(
                tool.description
                    .as_deref()
                    .unwrap_or("")
                    .contains("content_b64"),
                "`{name}` must be the in-band tool: {:?}",
                tool.description
            );
        }

        // With no report loaded the tool refuses with its own message:
        // method_not_found would mean the dispatch arm is missing, and a path in
        // the output that the old synthesised command answered.
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

/// #591: writes one raw JSON-RPC line and reads one back over a duplex half,
/// bypassing rmcp's client `ServiceExt`, which always opens with `initialize`
/// and has no public API for a `server/discover` opener.
async fn send_line(write: &mut (impl tokio::io::AsyncWrite + Unpin), value: serde_json::Value) {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(&value).expect("serialize request");
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .expect("write request");
}

async fn recv_line(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> serde_json::Value {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read response");
    serde_json::from_str(&line).expect("parse response")
}

/// #591: a `server/discover` opener at the 2026-07-28 draft, carrying the
/// `_meta` fields the inline lifecycle requires, must succeed over stdio
/// rather than being refused with `-32022` — and the connection must stay
/// usable for a follow-up `tools/call`, which the inline lifecycle also
/// requires conforming `_meta` on (row B of the investigation).
#[tokio::test]
async fn discover_2026_07_28_succeeds_over_stdio_and_the_session_stays_usable() {
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server = build_server().await;
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });

    let (read_half, mut write_half) = tokio::io::split(client_io);
    let mut reader = tokio::io::BufReader::new(read_half);

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                }
            }
        }),
    )
    .await;

    let discover_response = recv_line(&mut reader).await;
    assert!(
        discover_response.get("error").is_none(),
        "discover must not be refused: {discover_response}"
    );
    let supported = discover_response["result"]["supportedVersions"]
        .as_array()
        .expect("supportedVersions array");
    assert!(
        supported.iter().any(|v| v.as_str() == Some("2026-07-28")),
        "stdio must advertise 2026-07-28: {discover_response}"
    );

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "whoami",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                }
            }
        }),
    )
    .await;

    let call_response = recv_line(&mut reader).await;
    assert!(
        call_response.get("error").is_none(),
        "the follow-up tool call must succeed on the same connection: {call_response}"
    );
    let text = call_response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content block");
    assert!(
        text.starts_with("User: testuser, app pid: "),
        "unexpected tool output: {text}"
    );

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// Serves a fresh [`McpServer`] over a duplex and hands back the raw halves of
/// the client end.
fn spawn_raw_server() -> (
    tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    spawn_raw_server_with_config(config)
}

/// [`spawn_raw_server`] with an explicit config, so wire tests can point
/// `[mcp] audit_log` at a temp file.
fn spawn_raw_server_with_config(
    config: Config,
) -> (
    tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
    let (server_io, client_io) = tokio::io::duplex(8192);
    let server_task = tokio::spawn(async move {
        let server = build_server_with_config(config).await;
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });
    let (read_half, write_half) = tokio::io::split(client_io);
    (
        tokio::io::BufReader::new(read_half),
        write_half,
        server_task,
    )
}

/// #657: 2026-07-28 requires `ttlMs` and `cacheScope` on every `tools/list`
/// result, and Claude Code loads no tools without them. `private` because the
/// list depends on the per-user `[mcp]` profile and allow/deny lists.
#[tokio::test]
async fn tools_list_carries_cache_hints_on_2026_07_28() {
    let (mut reader, mut write_half, server_task) = spawn_raw_server();

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                }
            }
        }),
    )
    .await;

    let response = recv_line(&mut reader).await;
    assert!(
        response.get("error").is_none(),
        "tools/list failed: {response}"
    );
    let result = &response["result"];
    assert!(
        result["tools"].as_array().is_some_and(|t| !t.is_empty()),
        "the list must not be empty: {response}"
    );
    assert_eq!(result["ttlMs"], 0, "ttlMs missing or wrong: {response}");
    assert_eq!(
        result["cacheScope"], "private",
        "cacheScope missing or wrong: {response}"
    );

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// #657: earlier revisions do not define the cache-hint fields, so a client
/// that negotiated 2025-11-25 must not receive them.
#[tokio::test]
async fn tools_list_omits_cache_hints_on_2025_11_25() {
    let (mut reader, mut write_half, server_task) = spawn_raw_server();

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "stdio-roundtrip", "version": "0" },
            }
        }),
    )
    .await;
    let init = recv_line(&mut reader).await;
    assert_eq!(
        init["result"]["protocolVersion"], "2025-11-25",
        "the server must negotiate the requested revision: {init}"
    );
    send_line(
        &mut write_half,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send_line(
        &mut write_half,
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;

    let response = recv_line(&mut reader).await;
    assert!(
        response.get("error").is_none(),
        "tools/list failed: {response}"
    );
    let result = response["result"].as_object().expect("result object");
    assert!(
        result["tools"].as_array().is_some_and(|t| !t.is_empty()),
        "the list must not be empty: {response}"
    );
    assert!(
        !result.contains_key("ttlMs"),
        "unexpected ttlMs: {response}"
    );
    assert!(
        !result.contains_key("cacheScope"),
        "unexpected cacheScope: {response}"
    );

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// #657: `initialize` and `server/discover` identify the server as `mtui-mcp`
/// at the crate version, not as the rmcp library underneath.
#[tokio::test]
async fn server_info_names_mtui_mcp_on_initialize_and_discover() {
    let info = with_client(|client| async move {
        let info = client
            .peer_info()
            .expect("initialize result")
            .server_info
            .clone()
            .expect("serverInfo");
        drop(client);
        info
    })
    .await;
    assert_eq!(info.name, "mtui-mcp");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));

    let (mut reader, mut write_half, server_task) = spawn_raw_server();
    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                }
            }
        }),
    )
    .await;
    let discover = recv_line(&mut reader).await;
    let server_info = &discover["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(server_info["name"], "mtui-mcp", "{discover}");
    assert_eq!(
        server_info["version"],
        env!("CARGO_PKG_VERSION"),
        "{discover}"
    );

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// #412: wire-supplied identity never populates the reserved principal.
///
/// Sends a raw `tools/call` carrying `_meta.principal` and the SEP-2575
/// per-request `io.modelcontextprotocol/clientInfo` — the client-controlled
/// channels that reach `call_tool` — and proves the audit record still carries
/// `principal: null`. The `initialize` `clientInfo` rides along for the
/// handshake and is pinned too: rmcp persists it on the peer
/// (`service/server.rs:690`, `peer.set_peer_info`), so `call_tool` can read it
/// through `context.peer.peer_info()` — the improvement #412 warns about. This
/// drives the real `call_tool`, so it pins the source: a later change filling
/// the field from any of these three channels fails here, the record carrying
/// `evil-client` (or `root` for `_meta.principal`). Tool *arguments* smuggling
/// the same
/// claim are pinned by `audit_principal_ignores_client_supplied_identity`
/// instead (`call_arguments` returns `arguments` only, so wire `_meta` never
/// enters kwargs and one test cannot cover both).
///
/// A real `RequestContext` is not constructible in tests (`Peer::new` is
/// rmcp-private and `RequestContext` is `#[non_exhaustive]` — the reason the
/// unit tests drive the `dispatch_audited` seam directly), so the wire is the
/// only way to reach `call_tool` with a context.
#[tokio::test]
async fn principal_stays_null_for_wire_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.jsonl");
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    config.mcp_audit_log = Some(path.clone());
    let (mut reader, mut write_half, server_task) = spawn_raw_server_with_config(config);

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "evil-client", "version": "0" },
            }
        }),
    )
    .await;
    let init = recv_line(&mut reader).await;
    assert_eq!(
        init["result"]["protocolVersion"], "2025-11-25",
        "the server must negotiate the requested revision: {init}"
    );
    send_line(
        &mut write_half,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "whoami",
                "arguments": {},
                "_meta": {
                    "principal": "root",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "evil-client",
                        "version": "0",
                    },
                },
            }
        }),
    )
    .await;

    let call = recv_line(&mut reader).await;
    assert!(call.get("error").is_none(), "the call must succeed: {call}");

    // The outcome record lands before the reply, so it is on disk now.
    let text = std::fs::read_to_string(&path).expect("sink readable");
    let mut lines = text.lines();
    let record: serde_json::Value =
        serde_json::from_str(lines.next().expect("one record")).expect("one object");
    assert!(
        lines.next().is_none(),
        "whoami is read-only: one record, got: {text}"
    );
    assert_eq!(record["event"], "call");
    assert_eq!(
        record.get("principal"),
        Some(&serde_json::Value::Null),
        "wire identity never populates the reserved field: {record:?}"
    );

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// #413: trace context on the wire (`params._meta`) reaches the audit
/// record.
///
/// Sends a raw `tools/call` carrying `traceparent` + `tracestate` in
/// `params._meta` — the SEP-414 wire shape — and proves both land on the
/// real `call_tool`, so it pins the *source*: the service loop moves wire
/// `_meta` into `context.meta` while the typed `request.meta` stays empty,
/// and reading the latter leaves the key absent. Reverting `call_tool` to
/// `request.meta` fails here.
///
/// A real `RequestContext` is not constructible in tests (`Peer::new` is
/// rmcp-private and `RequestContext` is `#[non_exhaustive]` — the reason the
/// unit tests drive the `dispatch_audited` seam directly), so the wire is the
/// only way to reach `call_tool` with a context.
#[tokio::test]
async fn trace_context_flows_from_wire_meta_to_audit_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.jsonl");
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    config.mcp_audit_log = Some(path.clone());
    let (mut reader, mut write_half, server_task) = spawn_raw_server_with_config(config);

    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "stdio-roundtrip", "version": "0" },
            }
        }),
    )
    .await;
    let init = recv_line(&mut reader).await;
    assert_eq!(
        init["result"]["protocolVersion"], "2025-11-25",
        "the server must negotiate the requested revision: {init}"
    );
    send_line(
        &mut write_half,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ca902b7-01";
    let tracestate = "rojo=00f067aa0ca902b7,congo=t61rcWkgMzE";
    send_line(
        &mut write_half,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "whoami",
                "arguments": {},
                "_meta": { "traceparent": traceparent, "tracestate": tracestate },
            }
        }),
    )
    .await;

    let call = recv_line(&mut reader).await;
    assert!(
        call.get("error").is_none(),
        "the traced call must succeed: {call}"
    );

    // The outcome record is awaited before the reply, so it is on disk now.
    let text = std::fs::read_to_string(&path).expect("sink readable");
    let mut lines = text.lines();
    let record: serde_json::Value =
        serde_json::from_str(lines.next().expect("one record")).expect("one object");
    assert!(
        lines.next().is_none(),
        "whoami is read-only: one record, got: {text}"
    );
    assert_eq!(record["event"], "call");
    assert_eq!(record["trace"], traceparent);
    assert_eq!(record["tracestate"], tracestate);

    drop(reader);
    drop(write_half);
    let _ = server_task.await;
}

/// A test-only command whose body blocks until its gate is released.
struct GatedProbe(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl mtui_core::Command for GatedProbe {
    fn name(&self) -> &'static str {
        "gated_wait_probe"
    }
    fn scope(&self) -> mtui_core::Scope {
        mtui_core::Scope::Fanout
    }
    async fn call(
        &self,
        _session: &mut mtui_core::Session,
        _args: &clap::ArgMatches,
    ) -> mtui_core::CommandResult {
        self.0.notified().await;
        Ok(())
    }
}

/// #624 over the wire: `wait_seconds` survives `call_arguments` and the server's
/// job branch, the held call really parks on the latch (the gate opens only
/// after the request is in flight), and the reply is the job's own `done` status
/// line. Routing the job branch unwrapped, or dropping `wait_seconds` from the
/// accepted arguments, fails here.
#[tokio::test]
async fn job_status_wait_seconds_round_trips_over_the_wire() {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    let provider = StdioProvider::new(config);
    let session = provider.get_or_create("<default>").await;

    // The job runs the gated probe; the served surface stays the real one.
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut probes = register_all();
    probes.register(Arc::new(GatedProbe(Arc::clone(&gate))));
    let job_id = session
        .start_job(Arc::new(probes), "gated_wait_probe", Vec::new())
        .expect("start_job succeeds");

    let server = McpServer::new(Arc::new(register_all()), Arc::clone(&session));
    let (server_io, client_io) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server serve");
        running.waiting().await.expect("server run");
    });
    let client = ().serve(client_io).await.expect("client serve/initialize");

    // Opens the gate while the call is held, so the reply can only be `done` if
    // the server actually waited for the job.
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        gate.notify_one();
    });

    let result = client
        .call_tool(
            CallToolRequestParams::new("job_status").with_arguments(
                serde_json::json!({ "job_id": job_id, "wait_seconds": 30 })
                    .as_object()
                    .cloned()
                    .expect("object"),
            ),
        )
        .await
        .expect("the held call answers");
    releaser.await.expect("releaser did not panic");

    assert_eq!(result.is_error, Some(false), "got: {:?}", result.content);
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .unwrap_or_default();
    assert!(
        text.starts_with(&format!("{job_id}: done (")),
        "the wait returned the finished job: {text:?}"
    );

    drop(client);
    let _ = server_task.await;
}
