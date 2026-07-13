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
//! Scope: this handler serves **one** [`McpSession`]. Under stdio a single
//! server instance serves the process's one client; under http the
//! [`SessionRegistry`](crate::provider::SessionRegistry) mints a fresh server
//! (hence a fresh isolated session) per MCP session. The testreport tools are
//! bead `mtui-rs-76e.8`; the job tools drive the session's background-job table
//! (bead `mtui-rs-76e.12`).

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
use crate::testreport_tools::{dispatch_testreport_tool, testreport_tool_descriptors};
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
    /// The set of hand-written testreport tool names (`testreport_read`/…).
    testreport_tools: Arc<HashSet<String>>,
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
        let testreport_descriptors = testreport_tool_descriptors();
        let mut routes = tool_routes(&registry);

        // The whole synthesised surface: command tools + the four job tools +
        // the hand-written testreport tools.
        let mut descriptors: Vec<ToolDescriptor> = command_descriptors
            .into_iter()
            .chain(job_descriptors)
            .chain(testreport_descriptors)
            .collect();

        // Token-budget passes, in upstream's order (main.py): slim every tool's
        // JSON schema of redundant boilerplate, then narrow the surface to the
        // configured profile. `full` with no allow/deny override is a no-op.
        for descriptor in &mut descriptors {
            descriptor.input_schema = crate::slim::slim_input_schema(&descriptor.input_schema);
        }
        let kept = crate::profiles::apply_profile(
            &mut descriptors,
            session.profile(),
            session.tools_allow(),
            session.tools_deny(),
        );
        let kept: HashSet<String> = kept.into_iter().collect();

        // Keep the dispatch views in lockstep with the (possibly filtered) tool
        // list so a profiled-out tool cannot still be called.
        routes.retain(|name, _| kept.contains(name));
        let job_tools: HashSet<String> = job_tool_descriptors()
            .iter()
            .map(|d| d.name.clone())
            .filter(|n| kept.contains(n))
            .collect();
        let testreport_tools: HashSet<String> = testreport_tool_descriptors()
            .iter()
            .map(|d| d.name.clone())
            .filter(|n| kept.contains(n))
            .collect();

        let tools: Vec<Tool> = descriptors.iter().map(descriptor_to_tool).collect();

        Self {
            registry,
            session,
            tools: Arc::new(tools),
            routes: Arc::new(routes),
            job_tools: Arc::new(job_tools),
            testreport_tools: Arc::new(testreport_tools),
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

        // A job-control tool: poll/control the session's background-job table.
        if self.job_tools.contains(&name) {
            return Ok(render(
                dispatch_job_tool(&self.session, &name, &kwargs).await,
            ));
        }

        // A hand-written testreport tool: acts directly on the loaded checkout.
        if self.testreport_tools.contains(&name) {
            let result = dispatch_testreport_tool(&self.session, &name, &kwargs)
                .await
                // Serialise the JSON object result to a single text block, matching
                // the command tools' single-content-block wire shape.
                .map(|v| v.to_string());
            return Ok(render(result));
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

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::Config;
    use mtui_core::register_all;

    fn server_with(config: Config) -> McpServer {
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        McpServer::new(registry, session)
    }

    fn tool_names(server: &McpServer) -> Vec<String> {
        server.tools.iter().map(|t| t.name.to_string()).collect()
    }

    #[test]
    fn full_profile_keeps_the_whole_surface() {
        // Default config == full profile, no overrides: every synthesised tool
        // plus job + testreport tools is present, and routes/tracking sets match.
        let server = server_with(Config::default());
        let names = tool_names(&server);
        assert!(names.iter().any(|n| n == "run"));
        assert!(names.iter().any(|n| n == "set_log_level"));
        assert!(names.iter().any(|n| n == "job_list"));
        assert!(names.iter().any(|n| n == "testreport_read"));
        assert!(server.routes.contains_key("run"));
        assert!(server.job_tools.contains("job_list"));
        assert!(server.testreport_tools.contains("testreport_read"));
    }

    #[test]
    fn core_profile_filters_tools_and_dispatch_views() {
        let mut config = Config::default();
        config.mcp_profile = "core".to_owned();
        let server = server_with(config);
        let names = tool_names(&server);

        // A core command stays; a non-core one is gone from the list *and* its route.
        assert!(names.iter().any(|n| n == "run"), "core tool kept");
        assert!(
            !names.iter().any(|n| n == "set_log_level"),
            "non-core tool removed from list"
        );
        assert!(server.routes.contains_key("run"), "core route kept");
        assert!(
            !server.routes.contains_key("set_log_level"),
            "non-core route pruned"
        );
        // Job + testreport tools are always core.
        assert!(server.job_tools.contains("job_list"));
        assert!(server.testreport_tools.contains("testreport_read"));
    }

    #[test]
    fn allow_and_deny_overrides_apply_at_construction() {
        let mut config = Config::default();
        config.mcp_profile = "core".to_owned();
        config.mcp_tools_allow = vec!["whoami".to_owned()]; // not in core
        config.mcp_tools_deny = vec!["run".to_owned()]; // in core
        let server = server_with(config);
        let names = tool_names(&server);

        assert!(names.iter().any(|n| n == "whoami"), "allow adds back");
        assert!(!names.iter().any(|n| n == "run"), "deny wins");
        assert!(!server.routes.contains_key("run"), "denied route pruned");
    }

    #[test]
    fn schemas_are_slimmed_on_the_wire() {
        // No tool schema carries a `title` keyword or a bare null arm after
        // construction — the slimming pass ran over the live surface.
        let server = server_with(Config::default());
        for tool in server.tools.iter() {
            let blob = serde_json::to_string(&*tool.input_schema).unwrap();
            assert!(
                !blob.contains("\"title\""),
                "{} kept a title keyword",
                tool.name
            );
            assert!(
                !blob.contains("{\"type\":\"null\"}"),
                "{} kept a null arm",
                tool.name
            );
        }
    }
}
