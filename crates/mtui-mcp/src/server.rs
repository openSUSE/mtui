//! Spike (P7.1) walking-skeleton MCP server.
//!
//! Proves the **runtime tool-registration** approach against rmcp 2.x: a
//! hand-written [`ServerHandler`] whose [`list_tools`](ServerHandler::list_tools)
//! and [`call_tool`](ServerHandler::call_tool) are built at *runtime* from the
//! command [`Registry`], with a *runtime*-constructed JSON input schema. This is
//! the Rust-idiomatic equivalent of upstream Python's dynamic FastMCP
//! registration: the enumeration axis is forced to runtime (the tool set is the
//! runtime `Registry` value), so we introspect it at runtime rather than via a
//! compile-time `#[tool]` macro.
//!
//! Scope is deliberately one tool (`whoami`): full synthesis over the registry,
//! deny-list filtering, the clap→schema converter, argv fidelity, and the
//! testreport tools are the *following* Phase-7 subtasks. See
//! `plans/mtui-rs-76e.1-rmcp-spike.md`.

use std::sync::Arc;

use mtui_core::Registry;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value};

use crate::session::McpSession;

/// The single command this spike synthesises a tool for.
const SPIKE_TOOL: &str = "whoami";

/// A minimal MCP server backing exactly one auto-generated tool.
///
/// Holds the command [`Registry`] plus the [`McpSession`] the tool dispatches
/// against. `McpSession` guards the underlying `Session` behind a mutex (because
/// [`mtui_core::dispatch_argv`] needs `&mut Session` while `ServerHandler`'s methods take
/// `&self`) and owns the shared-buffer sink that captures the command's display
/// output (see [`crate::session`] / [`crate::capture`]).
#[derive(Clone)]
pub struct SpikeServer {
    registry: Arc<Registry>,
    session: Arc<McpSession>,
}

impl SpikeServer {
    /// Builds the spike server from a registry and the client's session (as
    /// resolved through a [`crate::provider::SessionProvider`]).
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<McpSession>) -> Self {
        Self { registry, session }
    }

    /// The runtime-built input schema for the spike tool: an object with no
    /// properties. Proves schema construction happens at runtime (from
    /// `serde_json`), not via a derived type — the real clap→schema converter is
    /// P7.4.
    fn empty_object_schema() -> Map<String, Value> {
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(Map::new()));
        schema
    }
}

impl ServerHandler for SpikeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Runtime enumeration: look the command up in the live registry and
        // build its descriptor now. (Spike: one command; P7.6 iterates all.)
        let command = self.registry.get(SPIKE_TOOL).ok_or_else(|| {
            McpError::internal_error(format!("command not registered: {SPIKE_TOOL}"), None)
        })?;
        let description = command.about().unwrap_or("").to_owned();
        let tool = Tool::new(
            command.name().to_owned(),
            description,
            Arc::new(Self::empty_object_schema()),
        );
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref();
        if name != SPIKE_TOOL {
            return Err(McpError::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }

        // Reconstruct argv from kwargs and dispatch through the *same* engine
        // entry the REPL uses. The spike tool takes no args, so argv is empty;
        // the real kwargs→argv reconstruction (REMAINDER, subparsers) is P7.5.
        let argv: Vec<String> = Vec::new();

        // Dispatch through the session's central primitive (drain → lock →
        // dispatch → capture → output-cap; P7.3). On failure the error already
        // carries the (capped) stdout produced before the failure.
        match self.session.run_command(&self.registry, name, &argv).await {
            Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{}{err}",
                err.stdout
            ))])),
        }
    }
}
