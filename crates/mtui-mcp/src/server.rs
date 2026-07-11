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

use mtui_core::{Registry, Session, dispatch_argv};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

/// The single command this spike synthesises a tool for.
const SPIKE_TOOL: &str = "whoami";

/// A minimal MCP server backing exactly one auto-generated tool.
///
/// Holds the command [`Registry`] plus the [`Session`] the tool dispatches
/// against. The session is behind a [`Mutex`] because [`dispatch_argv`] needs
/// `&mut Session` while `ServerHandler`'s methods take `&self`. Capturing the
/// display output is done via a session built with a shared-buffer sink (see
/// [`crate::capture`]).
#[derive(Clone)]
pub struct SpikeServer {
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
    /// The shared sink the session writes to; drained per `call_tool`.
    output: crate::capture::SharedBuf,
}

impl SpikeServer {
    /// Builds the spike server from a registry, session, and the session's
    /// capture buffer (the three come paired from [`crate::capture::session`]).
    #[must_use]
    pub fn new(
        registry: Arc<Registry>,
        session: Session,
        output: crate::capture::SharedBuf,
    ) -> Self {
        Self {
            registry,
            session: Arc::new(Mutex::new(session)),
            output,
        }
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

        // Drain any stale output, run under the session lock, then read back
        // what the command printed to the captured display sink.
        let _ = self.output.take();
        let result = {
            let mut session = self.session.lock().await;
            dispatch_argv(&self.registry, &mut session, name, &argv).await
        };
        let text = self.output.take();

        match result {
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{text}{err}"
            ))])),
        }
    }
}
