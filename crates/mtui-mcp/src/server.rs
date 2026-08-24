//! The production MCP server handler.
//!
//! A hand-written [`ServerHandler`] whose [`list_tools`](ServerHandler::list_tools)
//! and [`call_tool`](ServerHandler::call_tool) are built at *runtime* from the
//! command [`Registry`], built dynamically rather than declared per tool, and
//! synthesises the **full** tool surface via
//! [`crate::tools`].
//!
//! On construction the server precomputes, once:
//!
//! * the `rmcp::model::Tool` list (command tools from [`build_tools`] + the four
//!   job tools from [`job_tool_descriptors`]), each carrying a `readOnlyHint`;
//! * the tool-name → [`ToolRoute`] map from `tool_routes`, so a call dispatches
//!   through the *same* engine entry the REPL uses.
//!
//! Deny-listed commands never enter the surface — [`build_tools`]
//! filters them — so a `call_tool` for e.g. `shell`/`edit` resolves to no route
//! and returns `method_not_found`.
//!
//! Scope: this handler serves **one** [`McpSession`]. Under stdio a single
//! server instance serves the process's one client; under http the
//! [`SessionRegistry`](crate::provider::SessionRegistry) mints a fresh server
//! (hence a fresh isolated session) per MCP session. The testreport tools are
//! hand-written (not synthesised from the registry); the job tools drive the
//! session's background-job table.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::future::Future;
use std::pin::Pin;

use mtui_core::Registry;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ProtocolVersion,
    ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, Peer, RoleServer};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::provider::SessionGuard;
use crate::session::{McpSession, ProgressSink};
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
    /// The set of hand-written in-band transfer tool names (`get`/`put`, #434).
    transfer_tools: Arc<HashSet<String>>,
    /// Last-touch timestamp (monotonic millis), bumped on every tool call and
    /// `list_tools`, read by the http registry's idle sweeper. Under stdio /
    /// tests it is a private throwaway atomic no sweeper observes.
    last_touch: Arc<AtomicU64>,
    /// RAII registry membership for an http-minted server: dropping it (when rmcp
    /// drops the server on session close, or the sweeper evicts it) frees a
    /// `session_cap` slot. `None` under stdio / tests (no registry). Held behind
    /// an `Arc` so `McpServer` stays `Clone` — the slot is freed when the last
    /// clone drops.
    _guard: Option<Arc<SessionGuard>>,
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
        // Untracked: stdio (one process, one client) and unit tests. No registry
        // membership, and a private last-touch atomic no sweeper reads.
        Self::build(registry, session, None, Arc::new(AtomicU64::new(0)))
    }

    /// Builds a server tracked by the http [`SessionRegistry`](crate::provider::SessionRegistry).
    ///
    /// Same synthesis as [`new`](Self::new), but the server carries the
    /// registry's per-session [`SessionGuard`] (dropping it frees a
    /// `session_cap` slot) and the shared `last_touch` timestamp the handler
    /// bumps on every tool call so the idle sweeper only reaps quiet sessions.
    #[must_use]
    pub(crate) fn new_tracked(
        registry: Arc<Registry>,
        session: Arc<McpSession>,
        guard: SessionGuard,
        last_touch: Arc<AtomicU64>,
    ) -> Self {
        Self::build(registry, session, Some(Arc::new(guard)), last_touch)
    }

    /// Shared synthesis body for [`new`](Self::new) / [`new_tracked`](Self::new_tracked).
    fn build(
        registry: Arc<Registry>,
        session: Arc<McpSession>,
        guard: Option<Arc<SessionGuard>>,
        last_touch: Arc<AtomicU64>,
    ) -> Self {
        let command_descriptors = build_tools(&registry);
        let job_descriptors = job_tool_descriptors();
        let testreport_descriptors = testreport_tool_descriptors();
        let transfer_descriptors = crate::transfer_tools::transfer_tool_descriptors();
        let mut routes = tool_routes(&registry);

        // The whole synthesised surface: command tools + the four job tools +
        // the hand-written testreport tools + the in-band get/put transfer
        // tools (#434 — their command forms are on MCP_DENYLIST, which is what
        // makes the same-name reuse here collision-free).
        let mut descriptors: Vec<ToolDescriptor> = command_descriptors
            .into_iter()
            .chain(job_descriptors)
            .chain(testreport_descriptors)
            .chain(transfer_descriptors)
            .collect();

        // Token-budget passes: slim every tool's JSON schema of redundant
        // boilerplate, then narrow the surface to the configured profile. `full`
        // with no allow/deny override is a no-op.
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
        let transfer_tools: HashSet<String> = crate::transfer_tools::transfer_tool_descriptors()
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
            transfer_tools: Arc::new(transfer_tools),
            last_touch,
            _guard: guard,
        }
    }

    /// Record activity on this session (monotonic millis), for the idle sweeper.
    ///
    /// Called at the top of the request handlers our server actually sees
    /// (`call_tool` / `list_tools`). Under stdio / tests the atomic is private
    /// and unobserved, so the bump is a cheap no-op consequence.
    fn touch(&self) {
        self.last_touch
            .store(crate::provider::now_millis(), Ordering::Relaxed);
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

/// The rmcp-backed [`ProgressSink`]: sends `notifications/progress` back to the
/// client for the in-flight tool call.
///
/// Built in [`call_tool`](ServerHandler::call_tool) from the request's
/// [`RequestContext`] — the cloned [`Peer`] plus the client-supplied
/// `progressToken`. It exists only when the client actually requested progress
/// (without a token there is nothing to notify, so we simply do not build the
/// sink), and it swallows transport failures so a flaky client can
/// never mask the command's result.
struct PeerProgressSink {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

impl ProgressSink for PeerProgressSink {
    fn report<'a>(
        &'a self,
        progress: f64,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let param = ProgressNotificationParam::new(self.token.clone(), progress)
                .with_message(message.to_owned());
            if let Err(err) = self.peer.notify_progress(param).await {
                tracing::debug!("progress notification failed: {err}");
            }
        })
    }
}

/// The protocol revisions `initialize` and `server/discover` negotiate down to:
/// [`rmcp::model::ProtocolVersion::KNOWN_VERSIONS`] minus `V_2026_07_28`.
///
/// mtui's http per-client isolation *is* the legacy session model: rmcp calls
/// [`crate::provider::SessionRegistry::try_make_server`] once per
/// `Mcp-Session-Id` session, and the minted [`McpServer`] owns that client's
/// [`McpSession`]. Revision 2026-07-28 removes protocol-level sessions and is
/// served statelessly (a throwaway session per request) regardless of
/// `legacy_session_mode`, so it must never be among the versions a client can
/// negotiate. A client that asks for it gets `-32022` and falls back to one of
/// these four.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
];

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.touch();
        Ok(ListToolsResult::with_all_items((*self.tools).clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.touch();
        let name = request.name.as_ref().to_owned();
        let kwargs = call_arguments(&request);

        // A slow foreground tool call emits `notifications/progress` heartbeats so
        // the client does not time out. Build the sink only when the client
        // supplied a `progressToken` (without one there is nothing to notify, so
        // the heartbeat costs nothing). Job-control tools are fast and
        // stay unwrapped.
        let sink: Option<PeerProgressSink> =
            context
                .meta
                .get_progress_token()
                .map(|token| PeerProgressSink {
                    peer: context.peer.clone(),
                    token,
                });
        let sink = sink.as_ref().map(|s| s as &dyn ProgressSink);

        // A job-control tool: poll/control the session's background-job table.
        if self.job_tools.contains(&name) {
            return Ok(render(dispatch_job_tool(&self.session, &name, &kwargs).await).into());
        }

        // A hand-written testreport tool: acts directly on the loaded checkout.
        if self.testreport_tools.contains(&name) {
            let Some(result) = cancellable(
                dispatch_testreport_tool(&self.session, &name, &kwargs, sink),
                &context.ct,
            )
            .await
            else {
                return Err(cancelled_error());
            };
            // Serialise the JSON object result to a single text block, matching
            // the command tools' single-content-block wire shape.
            return Ok(render(result.map(|v| v.to_string())).into());
        }

        // A hand-written in-band transfer tool (get/put, #434).
        if self.transfer_tools.contains(&name) {
            let Some(result) = cancellable(
                crate::transfer_tools::dispatch_transfer_tool(&self.session, &name, &kwargs, sink),
                &context.ct,
            )
            .await
            else {
                return Err(cancelled_error());
            };
            return Ok(render(result.map(|v| v.to_string())).into());
        }

        // A synthesised command tool: dispatch through the shared engine.
        if let Some(route) = self.routes.get(&name) {
            let Some(result) = cancellable(
                dispatch_tool(&self.registry, &self.session, route, &kwargs, sink),
                &context.ct,
            )
            .await
            else {
                return Err(cancelled_error());
            };
            return Ok(render(result).into());
        }

        // Unknown / deny-listed name: no route was synthesised for it.
        Err(McpError::method_not_found::<
            rmcp::model::CallToolRequestMethod,
        >())
    }
}

/// Races `fut` against the client's `notifications/cancelled` signal,
/// `biased` so a future that is already resolved is never starved by the
/// cancellation branch. Returns `None` when `ct` fires first.
///
/// This only ever fires for a client that explicitly cancels — on stdio
/// (mtui-mcp's default transport) there is no per-request connection to drop,
/// and rmcp's client-disconnect cancellation exists only on the stateless HTTP
/// paths mtui declines (see `docs/src/mcp.md`). The job-control branch is
/// deliberately left unwrapped: it is fast, and cancelling `job_cancel` itself
/// makes no sense.
async fn cancellable<T>(fut: impl Future<Output = T>, ct: &CancellationToken) -> Option<T> {
    tokio::select! {
        biased;
        result = fut => Some(result),
        () = ct.cancelled() => None,
    }
}

/// The error returned in place of a cancelled call's result.
///
/// rmcp has already dropped this request's id from its cancellation-token
/// pool once the notification arrives, so the caller-visible response is
/// discarded either way (`service.rs`'s "dropping response for cancelled
/// request"); returning an explicit error here — rather than fabricating a
/// success — keeps the code honest about what happened.
fn cancelled_error() -> McpError {
    tracing::info!("MCP tool call cancelled by client notification");
    McpError::internal_error("request cancelled by client", None)
}

/// Render a dispatch result into a [`CallToolResult`].
///
/// Success returns the captured (output-capped) stdout; failure returns an
/// error result whose text is the captured stdout followed by the error
/// summary, preserving any output produced before the failure.
fn render(result: Result<String, crate::session::McpCommandError>) -> CallToolResult {
    match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("{}{err}", err.stdout))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::SessionRegistry;
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
        assert!(!names.iter().any(|n| n == "shell"));
        assert!(server.routes.contains_key("run"));
        assert!(!server.routes.contains_key("shell"));
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
    fn tools_allow_cannot_restore_shell() {
        let mut config = Config::default();
        config.mcp_profile = "core".to_owned();
        config.mcp_tools_allow = vec!["shell".to_owned()];
        let server = server_with(config);

        assert!(!tool_names(&server).iter().any(|n| n == "shell"));
        assert!(!server.routes.contains_key("shell"));
    }

    #[test]
    fn http_factory_server_denies_shell() {
        let registry = SessionRegistry::new(Arc::new(register_all()), Config::default());
        let server = registry.try_make_server().expect("http server");

        assert!(!tool_names(&server).iter().any(|n| n == "shell"));
        assert!(!server.routes.contains_key("shell"));
    }

    #[test]
    fn supported_protocol_versions_excludes_the_stateless_revision() {
        // 2026-07-28 is served statelessly regardless of `legacy_session_mode`
        // (rmcp classifies it from the request, not the config), so it must
        // never be among the versions a client can negotiate to. Anti-vacuity:
        // also assert the latest legacy revision *is* present, so an
        // accidentally emptied list cannot green this.
        let server = server_with(Config::default());
        let versions = server.supported_protocol_versions();
        assert!(!versions.contains(&rmcp::model::ProtocolVersion::V_2026_07_28));
        assert!(versions.contains(&rmcp::model::ProtocolVersion::V_2025_11_25));
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

    /// The property that matters for `notifications/cancelled` support: the
    /// `CommandLock` a cancelled dispatch was holding is released, not just
    /// that the call returns. Drives `cancellable` directly against
    /// `McpSession` — a real `RequestContext<RoleServer>` needs a live `Peer`,
    /// which is awkward to fake offline; the `call_tool` wiring around this
    /// helper is covered by inspection.
    #[tokio::test]
    async fn cancelling_a_dispatch_drops_the_future_and_releases_its_command_lock() {
        use std::sync::Mutex as StdMutex;

        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope, register_all};

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        /// A body blocked mid host-op that never observes any cancellation
        /// signal itself — only dropping its future can stop it, exactly the
        /// shape `cancellable` exists to handle.
        struct Stubborn(StdMutex<Option<tokio::sync::oneshot::Sender<()>>>);
        #[async_trait::async_trait]
        impl Command for Stubborn {
            fn name(&self) -> &'static str {
                "cancellable_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Fanout
            }
            async fn call(
                &self,
                _session: &mut mtui_core::Session,
                _args: &ArgMatches,
            ) -> CommandResult {
                if let Some(tx) = self.0.lock().expect("probe channel poisoned").take() {
                    let _ = tx.send(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                Ok(())
            }
        }

        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let session = McpSession::new(config);
        let mut registry = register_all();
        registry.register(Arc::new(Stubborn(StdMutex::new(Some(started_tx)))));
        let registry = Arc::new(registry);

        let ct = CancellationToken::new();
        let call = tokio::spawn({
            let session = Arc::clone(&session);
            let registry = Arc::clone(&registry);
            let ct = ct.clone();
            async move {
                cancellable(
                    session.run_command(&registry, "cancellable_probe", &[]),
                    &ct,
                )
                .await
            }
        });

        started_rx.await.expect("probe body started");
        ct.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("cancellable must return promptly, not hang on the parked body")
            .expect("spawned task did not panic");
        assert!(result.is_none(), "a cancelled dispatch must yield None");

        // The exclusive-path lock the parked probe held is released once its
        // future is dropped: a follow-up dispatch completes rather than
        // queuing forever behind a stranded hold.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_command(&registry, "whoami", &[]),
        )
        .await
        .expect("follow-up dispatch must not hang on a stranded lock")
        .expect("whoami succeeds");
        assert!(out.contains("testuser"), "got: {out}");
    }
}
