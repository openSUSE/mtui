//! The production MCP server handler (P7.7).
//!
//! A hand-written [`ServerHandler`] whose [`list_tools`](ServerHandler::list_tools)
//! and [`call_tool`](ServerHandler::call_tool) are built at *runtime* from the
//! command [`Registry`] — the Rust-idiomatic equivalent of upstream Python's
//! dynamic FastMCP registration. This grew out of the P7.1 spike (which proved
//! the runtime-registration approach against rmcp 2.x with a single hard-coded
//! `whoami` tool); it now synthesises the **full** tool surface via
//! [`crate::tools`].
//!
//! On construction the server precomputes, once:
//!
//! * the `rmcp::model::Tool` list (command tools from [`build_tools`] + the four
//!   job tools from [`job_tool_descriptors`]), each carrying a `readOnlyHint`;
//! * the tool-name → [`ToolRoute`] map from [`tool_routes`], so a call dispatches
//!   through the *same* engine entry the REPL uses.
//!
//! Deny-listed REPL-only commands never enter the surface — [`build_tools`]
//! filters them — so a `call_tool` for e.g. `shell`/`quit` resolves to no route
//! and returns `method_not_found`.
//!
//! Scope: this handler serves one [`McpSession`] (the stdio single-session
//! model). The http per-client session registry is bead `mtui-rs-76e.10`; the
//! testreport tools are `mtui-rs-76e.8`; the job tools are surfaced but their
//! handlers are stubbed until `mtui-rs-76e.12`.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use mtui_core::Registry;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::{Map, Value};

use crate::session::McpSession;
use crate::tools::{
    ToolDescriptor, ToolRoute, build_tools, dispatch_job_tool, dispatch_tool, job_tool_descriptors,
    tool_routes,
};

/// The runtime-synthesised MCP server backing one [`McpSession`].
///
/// Holds the command [`Registry`], the client's [`McpSession`], and the
/// precomputed tool list + route map. `McpSession` guards the underlying
/// `Session` behind a mutex (because [`mtui_core::dispatch_argv`] needs
/// `&mut Session` while `ServerHandler`'s methods take `&self`) and owns the
/// capture sink for a command's display output.
#[derive(Clone)]
pub struct McpServer {
    registry: Arc<Registry>,
    session: Arc<McpSession>,
    /// The full tool surface, built once at construction.
    tools: Arc<Vec<Tool>>,
    /// tool-name → command route, for dispatching command tools.
    routes: Arc<BTreeMap<String, ToolRoute>>,
    /// The set of job-control tool names (`job_list`/…), for dispatch routing.
    job_tools: Arc<HashSet<String>>,
}

impl McpServer {
    /// Builds the server from a registry and the client's session (as resolved
    /// through a [`crate::provider::SessionProvider`]).
    ///
    /// Synthesises the full tool surface once: command tools + the four job
    /// tools, each converted to an `rmcp::model::Tool` with its `readOnlyHint`,
    /// plus the route map used by [`call_tool`](ServerHandler::call_tool).
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<McpSession>) -> Self {
        let command_descriptors = build_tools(&registry);
        let job_descriptors = job_tool_descriptors();
        let routes = tool_routes(&registry);

        let job_tools: HashSet<String> = job_descriptors.iter().map(|d| d.name.clone()).collect();

        let tools: Vec<Tool> = command_descriptors
            .iter()
            .chain(job_descriptors.iter())
            .map(descriptor_to_tool)
            .collect();

        Self {
            registry,
            session,
            tools: Arc::new(tools),
            routes: Arc::new(routes),
            job_tools: Arc::new(job_tools),
        }
    }
}

/// Convert a transport-free [`ToolDescriptor`] into an `rmcp::model::Tool`,
/// carrying the conservative `readOnlyHint`.
fn descriptor_to_tool(descriptor: &ToolDescriptor) -> Tool {
    Tool::new(
        descriptor.name.clone(),
        descriptor.description.clone(),
        Arc::new(descriptor.input_schema.clone()),
    )
    .with_annotations(ToolAnnotations::new().read_only(descriptor.read_only))
}

/// Extract the tool-call arguments as a JSON object (empty when omitted).
fn call_arguments(request: &CallToolRequestParams) -> Map<String, Value> {
    request.arguments.clone().unwrap_or_default()
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items((*self.tools).clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref().to_owned();
        let kwargs = call_arguments(&request);

        // A job-control tool (stubbed until mtui-rs-76e.12).
        if self.job_tools.contains(&name) {
            return Ok(render(dispatch_job_tool(&name, &kwargs).await));
        }

        // A synthesised command tool: dispatch through the shared engine.
        if let Some(route) = self.routes.get(&name) {
            return Ok(render(
                dispatch_tool(&self.registry, &self.session, route, &kwargs).await,
            ));
        }

        // Unknown / deny-listed name: no route was synthesised for it.
        Err(McpError::method_not_found::<
            rmcp::model::CallToolRequestMethod,
        >())
    }
}

/// Render a dispatch result into a [`CallToolResult`].
///
/// Success returns the captured (output-capped) stdout; failure returns an error
/// result whose text is the captured stdout followed by the error summary — the
/// same envelope the P7.1 spike used, preserving any output produced before the
/// failure.
fn render(result: Result<String, crate::session::McpCommandError>) -> CallToolResult {
    match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("{}{err}", err.stdout))]),
    }
}

/// Backwards-compatible alias for the P7.1 spike name.
///
/// The spike round-trip test and early consumers referred to `SpikeServer`; the
/// production handler is [`McpServer`]. Kept as a thin alias so the existing test
/// keeps compiling while callers migrate.
pub type SpikeServer = McpServer;
