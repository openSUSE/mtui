//! The production MCP server handler.
//!
//! A hand-written [`ServerHandler`] whose
//! [`list_tools`](ServerHandler::list_tools) and
//! [`call_tool`](ServerHandler::call_tool) surfaces are synthesised at *runtime*
//! from the command [`Registry`] rather than declared per tool. On construction
//! it precomputes, once:
//!
//! * the `rmcp::model::Tool` list (command tools from [`build_tools`] + the four
//!   job tools from [`job_tool_descriptors`]), each carrying a `readOnlyHint`;
//! * the tool-name → [`ToolRoute`] map from `tool_routes`, so a call dispatches
//!   through the *same* engine entry the REPL uses.
//!
//! Deny-listed commands never enter the surface — [`build_tools`] filters them —
//! so a `call_tool` for e.g. `shell`/`edit` resolves to no route and returns
//! `method_not_found`.
//!
//! Scope: this handler serves **one** [`McpSession`]. Under stdio one server
//! instance serves the process's one client; under http the
//! [`SessionRegistry`](crate::provider::SessionRegistry) mints a fresh server —
//! hence a fresh isolated session — per MCP session. The testreport tools are
//! hand-written; the job tools drive the session's background-job table.

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
use crate::session::{AbortUnlock, McpSession, ProgressSink, ToolOutcome, forced_abort_note};
use crate::testreport_tools::{dispatch_testreport_tool, testreport_tool_descriptors};
use crate::tools::{
    ToolDescriptor, ToolRoute, build_tools, dispatch_job_tool, dispatch_tool, job_tool_descriptors,
    tool_routes,
};

/// The runtime-synthesised MCP server backing one [`McpSession`].
///
/// Holds the command [`Registry`], the client's [`McpSession`] and the
/// precomputed tool list + route map. `McpSession` guards the underlying
/// `Session` behind a mutex — [`mtui_core::dispatch_argv`] needs
/// `&mut Session` while `ServerHandler`'s methods take `&self` — and owns the
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
    /// Which transport this server was built for, deciding
    /// [`supported_protocol_versions`](ServerHandler::supported_protocol_versions).
    transport: Transport,
}

/// The transport an [`McpServer`] was built for.
///
/// Set once by the constructor that built it ([`McpServer::new`] → `Stdio`,
/// [`McpServer::new_tracked`] → `Http`) and never changed afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    Stdio,
    Http,
}

impl McpServer {
    /// Builds the server from a registry and the client's session (as resolved
    /// through a [`crate::provider::SessionProvider`]).
    ///
    /// Synthesises the full tool surface once, plus the route map
    /// [`call_tool`](ServerHandler::call_tool) uses.
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<McpSession>) -> Self {
        // Untracked: stdio (one process, one client) and unit tests.
        Self::build(
            registry,
            session,
            None,
            Arc::new(AtomicU64::new(0)),
            Transport::Stdio,
        )
    }

    /// Builds a server tracked by the http [`SessionRegistry`](crate::provider::SessionRegistry).
    ///
    /// Same synthesis as [`new`](Self::new), but carrying the registry's
    /// per-session [`SessionGuard`] (dropping it frees a `session_cap` slot) and
    /// the shared `last_touch` the handler bumps on every tool call, so the idle
    /// sweeper only reaps quiet sessions.
    #[must_use]
    pub(crate) fn new_tracked(
        registry: Arc<Registry>,
        session: Arc<McpSession>,
        guard: SessionGuard,
        last_touch: Arc<AtomicU64>,
    ) -> Self {
        Self::build(
            registry,
            session,
            Some(Arc::new(guard)),
            last_touch,
            Transport::Http,
        )
    }

    /// Shared synthesis body for [`new`](Self::new) / [`new_tracked`](Self::new_tracked).
    fn build(
        registry: Arc<Registry>,
        session: Arc<McpSession>,
        guard: Option<Arc<SessionGuard>>,
        last_touch: Arc<AtomicU64>,
        transport: Transport,
    ) -> Self {
        let command_descriptors = build_tools(&registry);
        let job_descriptors = job_tool_descriptors();
        let testreport_descriptors = testreport_tool_descriptors();
        let transfer_descriptors = crate::transfer_tools::transfer_tool_descriptors();
        let mut routes = tool_routes(&registry);

        // Command tools + the four job tools + the hand-written testreport tools
        // + the in-band get/put transfer tools (#434 — their command forms are on
        // MCP_DENYLIST, which makes the same-name reuse here collision-free).
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
            transport,
        }
    }

    /// Record activity on this session (monotonic millis), for the idle sweeper.
    ///
    /// Called at the top of `call_tool` / `list_tools`; under stdio and tests the
    /// atomic is private and unobserved.
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
/// Built in [`call_tool`](ServerHandler::call_tool) from the request's cloned
/// [`Peer`] plus the client-supplied `progressToken`, so it exists only when the
/// client actually requested progress. It swallows transport failures: a flaky
/// client must never mask the command's result.
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

/// The protocol revisions `initialize` and `server/discover` negotiate down to
/// on **http**: [`rmcp::model::ProtocolVersion::KNOWN_VERSIONS`] minus
/// `V_2026_07_28`.
///
/// Revision 2026-07-28 removes protocol-level sessions: rmcp's
/// `streamable_http_server` tower layer classifies any non-`initialize`
/// request carrying complete 2026-07-28 `_meta` as stateless and mints a
/// throwaway [`McpServer`] to serve it inline — bypassing
/// [`crate::provider::SessionRegistry::try_make_server`]'s one-server-per-
/// `Mcp-Session-Id` allocation entirely. That would tear down and rebuild the
/// per-session `McpSession` (its SSH connections, pool claims) on every such
/// request, so http must keep refusing the revision: a client that asks for it
/// gets `-32022` and falls back to one of these four.
///
/// This does **not** apply to stdio, which has one client per process and no
/// per-request session churn to protect — see [`SUPPORTED_PROTOCOL_VERSIONS_STDIO`].
const SUPPORTED_PROTOCOL_VERSIONS_HTTP: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
];

/// The protocol revisions `initialize` and `server/discover` negotiate down to
/// on **stdio**: every revision this rmcp build knows, including
/// `V_2026_07_28`.
///
/// stdio has exactly one client per process, so the inline (stateless)
/// lifecycle 2026-07-28 requests is harmless — there is no per-session state to
/// tear down. Refusing the revision here only breaks clients that open with
/// `server/discover` at 2026-07-28 and have no working fallback (#591).
const SUPPORTED_PROTOCOL_VERSIONS_STDIO: &[ProtocolVersion] = ProtocolVersion::KNOWN_VERSIONS;

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(match self.transport {
            Transport::Stdio => SUPPORTED_PROTOCOL_VERSIONS_STDIO,
            Transport::Http => SUPPORTED_PROTOCOL_VERSIONS_HTTP,
        })
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
        // W3C trace correlation (SEP-414): strict lowercase 55-byte
        // traceparent via `_meta` when the client supplied one; absent (or
        // invalid) otherwise. Documented as absent when unreachable.
        let traceparent = request
            .meta
            .as_ref()
            .and_then(|meta| meta.get_traceparent())
            .map(str::to_owned);

        // Heartbeats keep a slow foreground call from timing the client out.
        // Only built when the client supplied a `progressToken`; job-control
        // tools are fast and stay unwrapped.
        let sink: Option<PeerProgressSink> =
            context
                .meta
                .get_progress_token()
                .map(|token| PeerProgressSink {
                    peer: context.peer.clone(),
                    token,
                });
        let sink = sink.as_ref().map(|s| s as &dyn ProgressSink);

        self.dispatch_audited(&name, &kwargs, sink, &context.ct, traceparent.as_deref())
            .await
    }
}

impl McpServer {
    /// The audited dispatch behind [`call_tool`](ServerHandler::call_tool):
    /// the single chokepoint every tool call funnels through.
    ///
    /// With `[mcp] audit_log` set and/or the `OTEL_*` endpoint on, one record
    /// per call is persisted before the response returns — for foreground and
    /// backgrounded calls, for failures and unknown tools alike — and a call
    /// the sinks cannot record is refused instead of proceeding unrecorded.
    /// OTLP-only (endpoint set, `audit_log` unset) still builds the JSONL
    /// line in memory and uses it verbatim as the OTLP body. With neither
    /// sink this is dispatch verbatim: behaviour and output are byte-identical.
    ///
    /// Test seam: unit tests drive this directly with a fresh token and no
    /// sink, since a real `RequestContext` needs a peer.
    pub(crate) async fn dispatch_audited(
        &self,
        name: &str,
        kwargs: &Map<String, Value>,
        sink: Option<&dyn ProgressSink>,
        client_ct: &CancellationToken,
        traceparent: Option<&str>,
    ) -> Result<CallToolResponse, McpError> {
        use crate::audit::{
            AUDIT_SCHEMA_VERSION, AuditEvent, AuditOutcome, refuse_error, sanitize_args,
        };

        let start = std::time::Instant::now();
        let started_ms = crate::audit::now_millis();
        // Strict lowercase 55-byte traceparent when the client supplied one;
        // invalid values are ignored (no value is ever logged).
        let trace = traceparent.and_then(crate::otel::parse_traceparent);
        if traceparent.is_some() && trace.is_none() {
            tracing::debug!("ignoring invalid traceparent");
        }
        let file_on = self.session.audit_log().is_some();
        let otel_on = self.session.otel().is_some();
        // Refuse before running when a sink is already unwritable/unhealthy: a
        // mutation this consequential must not proceed unrecorded. (A refused
        // call leaves no record — there is nowhere to put one.) The file
        // pre-flight runs on the blocking pool: a down/slow disk must not
        // stall the worker.
        if let Some(audit) = self.session.audit_log()
            && let Err(err) = audit.check_writable_async().await
        {
            return Err(refuse_error(&err));
        }
        if let Some(otel) = self.session.otel()
            && !otel.is_healthy()
        {
            return Err(otel_refuse_error("otlp unhealthy"));
        }
        let seq = if file_on || otel_on {
            Some(crate::audit::next_seq())
        } else {
            None
        };

        // Every arm below resolves to one audited outcome; the record is
        // written once at the tail.
        let mut event = AuditEvent::Call;
        let mut rrids: Vec<String> = Vec::new();
        let mut job_ids: Vec<String> = Vec::new();
        let outcome: AuditOutcome;
        let result: Result<CallToolResponse, McpError>;

        // A job-control tool: poll/control the session's background-job table.
        if self.job_tools.contains(name) {
            let dispatched = dispatch_job_tool(&self.session, name, kwargs).await;
            outcome = if dispatched.is_ok() {
                AuditOutcome::Ok
            } else {
                AuditOutcome::Error
            };
            result = Ok(render(dispatched).into());
        }
        // Acts directly on the loaded checkout. Neither this nor the transfer
        // branch below dispatches through the engine, so neither can hold
        // `/var/lock/mtui.lock`: a plain drop on cancel strands nothing.
        else if self.testreport_tools.contains(name) {
            // Audit-only scope: skipped when no sink is on, so unaudited
            // dispatch never takes the session mutex for the record (#613).
            if seq.is_some() {
                let template = kwargs.get("template").and_then(Value::as_str);
                rrids = self.session.audit_template_scope(template).await;
            }
            let dispatched = cancellable(
                dispatch_testreport_tool(&self.session, name, kwargs, sink),
                client_ct,
            )
            .await;
            match dispatched {
                None => {
                    outcome = AuditOutcome::Error;
                    result = Err(cancelled_error(None));
                }
                Some(dispatched) => {
                    outcome = if dispatched.is_ok() {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Error
                    };
                    // One text block, matching the command tools' wire shape.
                    result = Ok(render(dispatched.map(|v| v.to_string())).into());
                }
            }
        }
        // A hand-written in-band transfer tool (get/put, #434).
        else if self.transfer_tools.contains(name) {
            if seq.is_some() {
                let template = kwargs.get("template").and_then(Value::as_str);
                rrids = self.session.audit_template_scope(template).await;
            }
            let dispatched = cancellable(
                crate::transfer_tools::dispatch_transfer_tool(&self.session, name, kwargs, sink),
                client_ct,
            )
            .await;
            match dispatched {
                None => {
                    outcome = AuditOutcome::Error;
                    result = Err(cancelled_error(None));
                }
                Some(dispatched) => {
                    outcome = if dispatched.is_ok() {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Error
                    };
                    result = Ok(render(dispatched.map(|v| v.to_string())).into());
                }
            }
        }
        // Dispatch through the shared engine. The one branch that can hold
        // `/var/lock/mtui.lock` on a real host, so a plain `cancellable` drop
        // would strand it: `dispatch_tool` gets the client's own token and runs
        // the two-stage cancel/abort/unlock sequence `job_cancel` uses.
        else if let Some(route) = self.routes.get(name) {
            let dispatched = dispatch_tool(
                &self.registry,
                &self.session,
                route,
                kwargs,
                sink,
                Some(client_ct),
            )
            .await;
            rrids = dispatched.rrids;
            match dispatched.outcome {
                ToolOutcome::Completed(inner) => {
                    outcome = if inner.is_ok() {
                        AuditOutcome::Ok
                    } else {
                        AuditOutcome::Error
                    };
                    if !dispatched.jobs.is_empty() {
                        event = AuditEvent::Dispatch;
                        job_ids = dispatched.jobs;
                    }
                    result = Ok(render(inner).into());
                }
                ToolOutcome::Aborted(unlock) => {
                    outcome = AuditOutcome::Error;
                    result = Err(cancelled_error(Some(&unlock)));
                }
            }
        }
        // Unknown / deny-listed name: no route was synthesised for it.
        else {
            outcome = AuditOutcome::UnknownTool;
            result = Err(McpError::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }

        if let Some(seq) = seq {
            let hosts = self.session.audit_hosts(&rrids).await;
            let response_bytes = response_bytes_of(&result);
            let mut record = serde_json::json!({
                "v": AUDIT_SCHEMA_VERSION,
                "ts": started_ms,
                "seq": seq,
                "session": self.session.id(),
                "transport": self.session.transport(),
                "event": event.as_str(),
                "tool": crate::audit::cap_str(name),
                "args": sanitize_args(name, kwargs),
                "outcome": outcome.as_str(),
                "duration_ms": start.elapsed().as_millis() as u64,
                "rrids": rrids,
                "hosts": hosts,
            });
            if event == AuditEvent::Dispatch {
                let capped: Vec<String> =
                    job_ids.iter().map(|id| crate::audit::cap_str(id)).collect();
                record["job_ids"] = serde_json::json!(capped);
            }
            if let Some(traceparent) = traceparent
                && trace.is_some()
            {
                record["trace"] = serde_json::json!(traceparent);
            }
            if let Some(bytes) = response_bytes {
                record["response_bytes"] = serde_json::json!(bytes);
            }
            // File first, seq order. The sink was writable at pre-flight, so
            // this fails only on a race: refuse in place of the result rather
            // than answering unrecorded. Off the worker via `spawn_blocking`.
            if let Some(audit) = self.session.audit_log()
                && let Err(err) = audit.append_async(record.clone()).await
            {
                return Err(refuse_error(&err));
            }
            // OTLP second, same seq and verbatim line. A race here (full or
            // newly unhealthy) refuses even though the file already holds the
            // record — the file stays as the durable truth and the exporter
            // reports an `audit_gap` on recovery.
            if let Some(otel) = self.session.otel() {
                let line = serde_json::to_string(&record).unwrap_or_default();
                if line.is_empty() {
                    return Err(otel_refuse_error(
                        crate::otel::ExportReason::Encode.as_str(),
                    ));
                }
                let queued = crate::otel::QueuedAudit {
                    seq,
                    jsonl: line,
                    tool: crate::audit::cap_str(name),
                    outcome: outcome.as_str().to_owned(),
                    event: event.as_str().to_owned(),
                    transport: self.session.transport().to_owned(),
                    session_id: self.session.id(),
                    response_bytes,
                    trace,
                    time_nanos: crate::otel::now_nanos(),
                    extra_attrs: crate::otel::kwarg_otlp_attrs(name, kwargs),
                };
                if let Err(reason) = otel.enqueue_audit(queued) {
                    let closed = match reason {
                        crate::otel::EnqueueError::Full => "otlp queue full",
                        crate::otel::EnqueueError::Unhealthy => "otlp unhealthy",
                    };
                    return Err(otel_refuse_error(closed));
                }
            }
        }
        result
    }
}

/// Refuse via the existing audit path with a closed-vocabulary reason: never
/// the endpoint, headers, or URL.
fn otel_refuse_error(reason: &'static str) -> McpError {
    crate::audit::refuse_error(&std::io::Error::other(reason))
}

/// Sized response length for the `mtui.response_bytes` attribute: the summed
/// text-block bytes of a completed tool result, else nothing.
fn response_bytes_of(result: &Result<CallToolResponse, McpError>) -> Option<usize> {
    let Ok(CallToolResponse::Complete(completed)) = result else {
        return None;
    };
    let mut bytes = 0usize;
    let mut sized = false;
    for block in &completed.content {
        if let Some(text) = block.as_text() {
            bytes = bytes.saturating_add(text.text.len());
            sized = true;
        }
    }
    sized.then_some(bytes)
}

/// Races `fut` against the client's `notifications/cancelled` signal,
/// `biased` so a future that is already resolved is never starved by the
/// cancellation branch. Returns `None` when `ct` fires first.
///
/// This only ever fires for a client that explicitly cancels: on stdio there is
/// no per-request connection to drop, and rmcp's client-disconnect cancellation
/// exists only on the stateless HTTP paths mtui declines (`docs/src/mcp.md`).
/// The job-control branch stays unwrapped — it is fast, and cancelling
/// `job_cancel` makes no sense.
///
/// For the testreport and transfer branches only: neither dispatches through the
/// engine, so dropping `fut` strands no `/var/lock/mtui.lock`. The
/// synthesised-command branch *can* hold that lock and routes through
/// [`McpSession::run_command_client_cancellable`](crate::session::McpSession::run_command_client_cancellable)
/// instead, which cancels cooperatively, allows a grace period, and only then
/// force-aborts and releases the lock on the dispatch's behalf.
async fn cancellable<T>(fut: impl Future<Output = T>, ct: &CancellationToken) -> Option<T> {
    tokio::select! {
        biased;
        result = fut => Some(result),
        () = ct.cancelled() => None,
    }
}

/// The error returned in place of a cancelled call's result.
///
/// rmcp drops this request's id from its cancellation-token pool once the
/// notification arrives, so the response is discarded either way; an explicit
/// error rather than a fabricated success keeps the code honest.
///
/// `unlock` is `Some` only for a force-aborted synthesised command tool
/// (`dispatch_tool` returning [`ToolOutcome::Aborted`]); its
/// [`forced_abort_note`] is appended so the client learns a host operation lock
/// may have been left behind, not merely that the call was cancelled. The
/// testreport/transfer branches cannot hold that lock and always pass `None`.
fn cancelled_error(unlock: Option<&AbortUnlock>) -> McpError {
    tracing::info!("MCP tool call cancelled by client notification");
    let message = match unlock {
        Some(unlock) => format!(
            "request cancelled by client ({})",
            forced_abort_note(unlock)
        ),
        None => "request cancelled by client".to_owned(),
    };
    McpError::internal_error(message, None)
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
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;
    use mtui_types::enums::TargetState;
    use serde_json::{Value, json};

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
        // Full profile, no overrides: every tool present, routes in lockstep.
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

        // A non-core command is gone from the list *and* its route.
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
    fn stdio_supported_protocol_versions_includes_the_stateless_revision() {
        // stdio has one client per process, so the inline 2026-07-28 lifecycle
        // is safe (#591) and must be negotiable.
        let server = server_with(Config::default());
        let versions = server.supported_protocol_versions();
        assert!(versions.contains(&rmcp::model::ProtocolVersion::V_2026_07_28));
    }

    #[test]
    fn http_supported_protocol_versions_excludes_the_stateless_revision() {
        // 2026-07-28 is served statelessly regardless of `legacy_session_mode`
        // (rmcp classifies it from the request), which would bypass the http
        // per-session `McpServer` allocation, so it must never be negotiable.
        // Anti-vacuity: the latest legacy revision must be present, so an
        // accidentally emptied list cannot green this.
        let registry = SessionRegistry::new(Arc::new(register_all()), Config::default());
        let server = registry.try_make_server().expect("http server");
        let versions = server.supported_protocol_versions();
        assert!(!versions.contains(&rmcp::model::ProtocolVersion::V_2026_07_28));
        assert!(versions.contains(&rmcp::model::ProtocolVersion::V_2025_11_25));
    }

    #[test]
    fn schemas_are_slimmed_on_the_wire() {
        // The slimming pass ran over the live surface.
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

    /// The **forced** case of `notifications/cancelled` on a synthesised command
    /// tool: a body blocked mid host-op never observes the cooperative signal, so
    /// `run_command_client_cancellable` gives it `CANCEL_GRACE`, then force-aborts
    /// it, releasing the `CommandLock` rather than stranding it. Drives
    /// `McpSession` directly, since a real `RequestContext<RoleServer>` needs a
    /// live `Peer`; the `call_tool` wiring around it is covered by inspection.
    #[tokio::test]
    async fn cancelling_a_dispatch_drops_the_future_and_releases_its_command_lock() {
        use std::sync::Mutex as StdMutex;

        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope, register_all};

        use crate::session::DEFAULT_PROGRESS_INTERVAL;

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        /// A body blocked mid host-op that observes no cancellation signal: only
        /// a forced abort can stop it, the shape the forced stage exists for.
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
                session
                    .run_command_client_cancellable(
                        &registry,
                        "cancellable_probe",
                        &[],
                        None,
                        DEFAULT_PROGRESS_INTERVAL,
                        &ct,
                    )
                    .await
            }
        });

        started_rx.await.expect("probe body started");
        ct.cancel();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("the forced sequence must return promptly, not hang on the parked body")
            .expect("spawned task did not panic");
        let ToolOutcome::Aborted(unlock) = outcome else {
            panic!("a body that never checks the seam must be force-aborted");
        };
        let err = cancelled_error(Some(&unlock));
        assert!(err.message.contains("forced abort"), "got: {}", err.message);

        // Dropping the parked probe's future releases the exclusive-path lock it
        // held, so a follow-up dispatch completes instead of queuing forever.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.run_command(&registry, "whoami", &[]),
        )
        .await
        .expect("follow-up dispatch must not hang on a stranded lock")
        .expect("whoami succeeds");
        assert!(out.contains("testuser"), "got: {out}");
    }

    // ---------------------------------------------------------- audit log (#411)

    const AUDIT_RRID: &str = "SUSE:Maintenance:1:1";
    const AUDIT_HOST: &str = "audit-host";

    /// A server with `[mcp] audit_log` pointed at a temp file, plus the
    /// session and paths the assertions read back.
    fn audited_server() -> (
        McpServer,
        Arc<McpSession>,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        let server = McpServer::new(registry, session.clone());
        (server, session, dir, path)
    }

    /// Load one template with one mock host and make it active.
    async fn seed_audit_host(session: &McpSession, mock: MockConnection) {
        let target = Target::with_connection(AUDIT_HOST, TargetState::Enabled, Box::new(mock));
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(AUDIT_RRID).expect("rrid"));
        report.base_mut().targets = HostsGroup::new(vec![target], false);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(AUDIT_RRID);
    }

    /// Drive the audited seam the way `call_tool` does (fresh token, no sink).
    async fn audited_call(
        server: &McpServer,
        tool: &str,
        kwargs: Value,
    ) -> Result<CallToolResponse, McpError> {
        server
            .dispatch_audited(
                tool,
                kwargs.as_object().expect("kwargs object"),
                None,
                &CancellationToken::new(),
                None,
            )
            .await
    }

    /// Read back every audit record.
    fn audit_records(path: &std::path::Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).expect("sink readable");
        text.lines()
            .map(|line| serde_json::from_str(line).expect("one object per line"))
            .collect()
    }

    /// Unwrap the completed result: the audited seam never answers the
    /// input-required/task variants.
    fn complete_result(response: &CallToolResponse) -> &CallToolResult {
        match response {
            CallToolResponse::Complete(result) => result,
            _ => panic!("audited seam answers Complete, got: {response:?}"),
        }
    }

    /// The text payload of a completed tool response.
    fn response_text(response: &CallToolResponse) -> String {
        complete_result(response)
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.to_string()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// Poll the sink until `n` records land: a background job's terminal
    /// record is written by a spawned worker after the dispatch answered.
    async fn await_records(path: &std::path::Path, n: usize) -> Vec<Value> {
        for _ in 0..2000 {
            let records = audit_records(path);
            if records.len() >= n {
                return records;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("sink never reached {n} records");
    }

    #[tokio::test]
    async fn audit_foreground_call_records_scope_and_outcome() {
        let (server, session, _dir, path) = audited_server();
        seed_audit_host(&session, MockConnection::new(AUDIT_HOST)).await;

        let response = audited_call(&server, "list_hosts", json!({}))
            .await
            .expect("list_hosts succeeds");
        let text = response_text(&response);
        assert!(text.contains(AUDIT_HOST), "the call really ran: {text}");

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "one call, one record");
        let record = &records[0];
        assert_eq!(record["v"], json!(1));
        assert_eq!(record["event"], json!("call"));
        assert_eq!(record["tool"], json!("list_hosts"));
        assert_eq!(record["outcome"], json!("ok"));
        assert_eq!(record["session"], json!(session.id()));
        assert_eq!(record["rrids"], json!([AUDIT_RRID]));
        assert_eq!(record["hosts"], json!([AUDIT_HOST]));
        assert!(record["ts"].as_u64().unwrap_or(0) > 0);
        assert!(record["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn audit_failing_call_records_error() {
        let (server, _session, _dir, path) = audited_server();
        // Unknown attribute: the engine fails the call, but MCP still answers
        // Ok with an error payload — the failure lives in the record.
        audited_call(
            &server,
            "config_set",
            json!({"attribute": "no_such_attr", "value": "x"}),
        )
        .await
        .expect("failing calls answer with an error payload, not a protocol error");

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "a failing call produces a record");
        assert_eq!(records[0]["tool"], json!("config_set"));
        assert_eq!(records[0]["outcome"], json!("error"));
    }

    #[tokio::test]
    async fn audit_unknown_tool_records_and_rejects() {
        let (server, _session, _dir, path) = audited_server();
        let err = audited_call(&server, "shell", json!({}))
            .await
            .expect_err("deny-listed tool is rejected");
        assert!(
            err.to_string().contains("-32601"),
            "method-not-found keeps its code: {err}"
        );

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "a refused call produces a record");
        assert_eq!(records[0]["tool"], json!("shell"));
        assert_eq!(records[0]["outcome"], json!("unknown-tool"));
        assert_eq!(records[0]["rrids"], json!(Value::Array(vec![])));
        assert_eq!(records[0]["hosts"], json!(Value::Array(vec![])));
    }

    #[tokio::test]
    async fn audit_config_set_secret_leaves_no_trace() {
        let (server, _session, _dir, path) = audited_server();
        let secret = "audit-secret-token-6f5e4d3c";
        audited_call(
            &server,
            "config_set",
            json!({"attribute": "gitea_token", "value": secret}),
        )
        .await
        .expect("config_set succeeds");

        let raw = std::fs::read_to_string(&path).expect("sink readable");
        assert!(
            !raw.contains(secret),
            "secret value must be unrepresentable in the record"
        );
        let record: Value = serde_json::from_str(raw.trim()).expect("one record");
        assert_eq!(record["args"]["attribute"], json!("gitea_token"));
        assert_eq!(record["args"]["value"], json!("<redacted>"));
        assert_eq!(record["args"]["secret"], json!(true));
    }

    #[tokio::test]
    async fn audit_background_run_writes_joinable_dispatch_and_terminal_records() {
        let (server, session, _dir, path) = audited_server();
        seed_audit_host(&session, MockConnection::new(AUDIT_HOST)).await;

        audited_call(
            &server,
            "run",
            json!({"command": ["true"], "background": true}),
        )
        .await
        .expect("background start answers");
        let jobs = session.job_list();
        assert_eq!(jobs.len(), 1, "one background job started");
        let job_id = jobs[0].id.clone();

        let records = await_records(&path, 2).await;
        assert_eq!(records.len(), 2, "dispatch + terminal, nothing else");
        let dispatch = &records[0];
        assert_eq!(dispatch["event"], json!("dispatch"));
        assert_eq!(dispatch["tool"], json!("run"));
        assert_eq!(dispatch["outcome"], json!("ok"));
        assert_eq!(dispatch["job_ids"], json!([job_id]));
        assert_eq!(dispatch["rrids"], json!([AUDIT_RRID]));
        assert_eq!(dispatch["hosts"], json!([AUDIT_HOST]));
        let terminal = &records[1];
        assert_eq!(terminal["event"], json!("terminal"));
        assert_eq!(terminal["tool"], json!("run"));
        assert_eq!(terminal["job_id"], json!(job_id));
        assert_eq!(terminal["job_state"], json!("done"));
        assert_eq!(terminal["outcome"], json!("ok"));
        assert_eq!(terminal["session"], dispatch["session"]);
        assert_eq!(terminal["rrids"], json!([AUDIT_RRID]));
        assert!(
            terminal.get("args").is_none(),
            "terminal carries no args: {terminal}"
        );
    }

    #[tokio::test]
    async fn audit_background_failure_writes_a_failed_terminal_record() {
        let (server, session, _dir, path) = audited_server();
        seed_audit_host(&session, MockConnection::new(AUDIT_HOST)).await;

        // `update` against a bare seeded report cannot proceed: the job fails,
        // and the terminal record must still join to its dispatch.
        audited_call(&server, "update", json!({"background": true}))
            .await
            .expect("background start answers");
        let jobs = session.job_list();
        assert_eq!(jobs.len(), 1, "one background job started");
        let job_id = jobs[0].id.clone();

        let records = await_records(&path, 2).await;
        assert_eq!(records.len(), 2, "dispatch + terminal, nothing else");
        assert_eq!(records[0]["event"], json!("dispatch"));
        assert_eq!(records[0]["job_ids"], json!([job_id]));
        let terminal = &records[1];
        assert_eq!(terminal["event"], json!("terminal"));
        assert_eq!(terminal["job_id"], json!(job_id));
        assert_eq!(terminal["job_state"], json!("failed"));
        assert_eq!(terminal["outcome"], json!("error"));
    }

    #[tokio::test]
    async fn audit_cancelled_job_writes_a_cancelled_terminal_record() {
        let (server, session, _dir, path) = audited_server();
        let mock =
            MockConnection::new(AUDIT_HOST).with_run_delay(std::time::Duration::from_secs(3));
        let probe = mock.clone();
        seed_audit_host(&session, mock).await;

        audited_call(
            &server,
            "run",
            json!({"command": ["true"], "background": true}),
        )
        .await
        .expect("background start answers");
        let jobs = session.job_list();
        assert_eq!(jobs.len(), 1, "one background job started");
        let job_id = jobs[0].id.clone();

        // Gate the cancel on the worker reaching the delayed command: the
        // claim then provably lands mid-flight, never on an already-done job.
        let mut saw_command = false;
        for _ in 0..2000 {
            if !probe.commands().is_empty() {
                saw_command = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(saw_command, "worker reached the host command");

        audited_call(&server, "job_cancel", json!({"job_id": job_id}))
            .await
            .expect("cancel answers");
        // Three records: the run dispatch, the run's terminal — written by the
        // cancel path, which owns it once claimed, so it precedes the cancel
        // call's own record in the file — and the job_cancel call. `ts` still
        // orders them causally.
        let records = await_records(&path, 3).await;
        assert_eq!(records.len(), 3, "no duplicate terminal record");
        assert_eq!(records[0]["event"], json!("dispatch"));
        assert_eq!(records[2]["tool"], json!("job_cancel"));
        assert_eq!(records[2]["outcome"], json!("ok"));
        let terminal = &records[1];
        assert_eq!(terminal["event"], json!("terminal"));
        assert_eq!(terminal["job_id"], json!(job_id));
        assert_eq!(terminal["job_state"], json!("cancelled"));
        assert_eq!(terminal["outcome"], json!("error"));
        assert!(terminal["ts"].as_u64() >= records[0]["ts"].as_u64());
    }

    #[tokio::test]
    async fn audit_unwritable_sink_refuses_before_dispatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        // A directory as the sink path: every open fails, so the pre-flight
        // refuses without running the call.
        config.mcp_audit_log = Some(dir.path().to_path_buf());
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        let server = McpServer::new(registry, session.clone());

        let err = audited_call(
            &server,
            "run",
            json!({"command": ["true"], "background": true}),
        )
        .await
        .expect_err("unwritable sink refuses the call");
        assert!(
            err.to_string().contains("audit log unavailable"),
            "refusal names the sink: {err}"
        );
        assert!(
            session.job_list().is_empty(),
            "refused before dispatch: no job was started"
        );
    }

    #[tokio::test]
    async fn audit_unset_sink_is_byte_identical() {
        async fn whoami(audit: Option<std::path::PathBuf>) -> CallToolResult {
            let mut config = Config::default();
            config.session_user = "testuser".to_owned();
            config.mcp_audit_log = audit;
            let registry = Arc::new(register_all());
            let session = McpSession::new(config);
            let server = McpServer::new(registry, session);
            let response = audited_call(&server, "whoami", json!({}))
                .await
                .expect("whoami succeeds");
            complete_result(&response).clone()
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let plain = whoami(None).await;
        let logged = whoami(Some(dir.path().join("audit.jsonl"))).await;
        assert_eq!(
            plain, logged,
            "enabling the sink must not change the wire response"
        );
    }

    // ------------------------------------------------- OTLP export (#411 ext)

    /// Mock OTLP collector capturing every POST body.
    async fn otlp_mock() -> (wiremock::MockServer, Arc<std::sync::Mutex<Vec<u8>>>) {
        let server = wiremock::MockServer::start().await;
        let bodies = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let seen = Arc::clone(&bodies);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/logs"))
            .respond_with(move |req: &wiremock::Request| {
                seen.lock()
                    .expect("body slot")
                    .extend_from_slice(req.body.as_slice());
                wiremock::ResponseTemplate::new(200)
            })
            .mount(&server)
            .await;
        (server, bodies)
    }

    fn otlp_exporter(endpoint: &str) -> Arc<crate::otel::OtelExporter> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("test client");
        crate::otel::OtelExporter::with_client(
            crate::otel::OtelConfig::for_tests(endpoint, "mtui"),
            client,
        )
    }

    /// Dispatch through the audited seam with an explicit traceparent.
    async fn audited_call_traced(
        server: &McpServer,
        tool: &str,
        kwargs: Value,
        traceparent: Option<&str>,
    ) -> Result<CallToolResponse, McpError> {
        server
            .dispatch_audited(
                tool,
                kwargs.as_object().expect("kwargs object"),
                None,
                &CancellationToken::new(),
                traceparent,
            )
            .await
    }

    #[tokio::test]
    async fn otlp_only_builds_jsonl_body_without_a_file() {
        let (mock, bodies) = otlp_mock().await;
        let endpoint = format!("{}/v1/logs", mock.uri());
        let exporter = otlp_exporter(&endpoint);
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        // No `audit_log`: OTLP-only mode still builds the JSONL line.
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);

        audited_call(&server, "whoami", json!({}))
            .await
            .expect("whoami succeeds");
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let body = bodies.lock().expect("body").clone();
        assert!(!body.is_empty(), "OTLP-only must still export");
        assert!(
            body.windows(b"whoami".len()).any(|w| w == b"whoami"),
            "protobuf carries the verbatim JSONL tool"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn both_sinks_share_one_seq_file_first() {
        let (mock, bodies) = otlp_mock().await;
        let endpoint = format!("{}/v1/logs", mock.uri());
        let exporter = otlp_exporter(&endpoint);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);

        audited_call(&server, "whoami", json!({}))
            .await
            .expect("whoami succeeds");
        let records = audit_records(&path);
        assert_eq!(records.len(), 1);
        let seq = records[0]["seq"].as_u64().expect("seq in file");
        assert_eq!(records[0]["transport"], json!("stdio"));
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let body = bodies.lock().expect("body").clone();
        let marker = format!("\"seq\":{seq}");
        assert!(
            body.windows(marker.len()).any(|w| w == marker.as_bytes()),
            "OTLP body carries the same seq {seq} as the file"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn unhealthy_otlp_refuses_before_dispatch() {
        let failing = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&failing)
            .await;
        let exporter = otlp_exporter(&format!("{}/v1/logs", failing.uri()));
        // Latch unhealthy with one failed batch.
        exporter
            .enqueue_audit(crate::otel::QueuedAudit {
                seq: crate::audit::next_seq(),
                jsonl: "{}".to_owned(),
                tool: "run".to_owned(),
                outcome: "ok".to_owned(),
                event: "call".to_owned(),
                transport: "stdio".to_owned(),
                session_id: 1,
                response_bytes: None,
                trace: None,
                time_nanos: crate::otel::now_nanos(),
                extra_attrs: Vec::new(),
            })
            .expect("enqueue while healthy");
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        assert!(!exporter.is_healthy(), "failed batch latches");

        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session.clone());
        let err = audited_call(&server, "whoami", json!({}))
            .await
            .expect_err("unhealthy OTLP refuses");
        assert!(
            err.to_string().contains("audit log unavailable"),
            "refusal reuses the file-sink path: {err}"
        );
        assert!(!err.to_string().contains("127.0.0.1"), "no endpoint leaks");
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn otlp_config_set_body_redacted() {
        // OTLP-only (no file): the exported protobuf body is the redacted
        // JSONL line — the secret never reaches the collector.
        let (mock, bodies) = otlp_mock().await;
        let endpoint = format!("{}/v1/logs", mock.uri());
        let exporter = otlp_exporter(&endpoint);
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);

        let secret = "otlp-secret-token-9f8e7d6c5b4a";
        audited_call(
            &server,
            "config_set",
            json!({"attribute": "gitea_token", "value": secret}),
        )
        .await
        .expect("config_set succeeds");
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let body = bodies.lock().expect("body").clone();
        assert!(!body.is_empty(), "OTLP-only must still export");
        assert!(
            !body.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "secret value absent from exported protobuf"
        );
        assert!(
            body.windows(b"<redacted>".len())
                .any(|w| w == b"<redacted>"),
            "redaction marker present in exported protobuf"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn otlp_queue_full_refuses_before_dispatch() {
        // Mirror of the unhealthy refusal: a filled queue refuses through the
        // same closed-vocabulary path, never leaking the endpoint.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("test client");
        // Unreachable endpoint, but the fill below finishes before the first
        // 500ms flush tick, so the latch is still healthy and the refusal is
        // `Full`, not `Unhealthy`.
        let exporter = crate::otel::OtelExporter::with_client(
            crate::otel::OtelConfig::for_tests("http://127.0.0.1:9/v1/logs", "mtui"),
            client,
        );
        for seq in 0..crate::otel::OTEL_QUEUE_CAP {
            exporter
                .enqueue_audit(crate::otel::QueuedAudit {
                    seq: seq as u64,
                    jsonl: "{}".to_owned(),
                    tool: "run".to_owned(),
                    outcome: "ok".to_owned(),
                    event: "call".to_owned(),
                    transport: "stdio".to_owned(),
                    session_id: 1,
                    response_bytes: None,
                    trace: None,
                    time_nanos: crate::otel::now_nanos(),
                    extra_attrs: Vec::new(),
                })
                .expect("queue accepts to capacity");
        }
        assert!(exporter.is_healthy(), "fill races the first flush tick");

        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);
        let err = audited_call(&server, "whoami", json!({}))
            .await
            .expect_err("full OTLP queue refuses");
        let msg = err.to_string();
        assert!(
            msg.contains("audit log unavailable"),
            "same refuse path: {msg}"
        );
        assert!(msg.contains("otlp queue full"), "closed reason: {msg}");
        assert!(!msg.contains("127.0.0.1"), "no endpoint leaks: {msg}");
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn audit_put_payload_is_fingerprinted_not_stored() {
        let (server, session, _dir, path) = audited_server();
        seed_audit_host(&session, MockConnection::new(AUDIT_HOST)).await;
        let secret = "PUT-SECRET-SSH-KEY-MATERIAL-7f3a";
        audited_call(
            &server,
            "put",
            json!({"filename": "id_rsa", "content": secret}),
        )
        .await
        .expect("put dispatches (record pins args either way)");
        let raw = std::fs::read_to_string(&path).expect("sink readable");
        assert!(!raw.contains(secret), "payload never verbatim: {raw}");
        let record: Value = serde_json::from_str(raw.trim()).expect("one record");
        assert_eq!(record["args"]["content"]["bytes"], json!(secret.len()));
        assert_eq!(
            record["args"]["content"]["sha256"]
                .as_str()
                .expect("hex")
                .len(),
            64
        );
        assert_eq!(record["args"]["filename"], json!("id_rsa"));
    }

    #[tokio::test]
    async fn unaudited_dispatch_skips_audit_scope_resolution() {
        // Gating pin for #613: the audit scope resolve is audit-only. The same
        // template-scoped call resolves with a sink on and stays empty with
        // auditing off, so unaudited dispatch never takes the session mutex
        // for the record.
        use crate::tools::tool_routes;
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("list_hosts").expect("list_hosts route");

        let plain = McpSession::new(Config::default());
        assert!(!plain.auditing(), "no sink means auditing off");
        seed_audit_host(&plain, MockConnection::new(AUDIT_HOST)).await;
        let dispatched = dispatch_tool(
            &registry,
            &plain,
            route,
            json!({}).as_object().expect("kwargs object"),
            None,
            None,
        )
        .await;
        assert!(
            dispatched.rrids.is_empty(),
            "unaudited dispatch records no scope: {:?}",
            dispatched.rrids
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(dir.path().join("audit.jsonl"));
        let audited = McpSession::new(config);
        assert!(audited.auditing(), "sink on means auditing on");
        seed_audit_host(&audited, MockConnection::new(AUDIT_HOST)).await;
        let dispatched = dispatch_tool(
            &registry,
            &audited,
            route,
            json!({}).as_object().expect("kwargs object"),
            None,
            None,
        )
        .await;
        assert_eq!(dispatched.rrids, vec![AUDIT_RRID.to_owned()]);
    }

    #[tokio::test]
    async fn traceparent_flows_to_record_and_otlp() {
        let (mock, bodies) = otlp_mock().await;
        let endpoint = format!("{}/v1/logs", mock.uri());
        let exporter = otlp_exporter(&endpoint);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);
        let traceparent = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";

        audited_call_traced(&server, "whoami", json!({}), Some(traceparent))
            .await
            .expect("traced call succeeds");
        let records = audit_records(&path);
        assert_eq!(records[0]["trace"], json!(traceparent));
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        let body = bodies.lock().expect("body").clone();
        // Raw trace bytes ride the OTLP record alongside the JSONL body.
        let trace_bytes = [0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd];
        assert!(
            body.windows(trace_bytes.len()).any(|w| w == trace_bytes),
            "trace_id bytes present in OTLP"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn response_bytes_sized_for_ok_absent_for_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        let server = McpServer::new(registry, session);

        audited_call(&server, "whoami", json!({}))
            .await
            .expect("whoami succeeds");
        let records = audit_records(&path);
        assert!(
            records[0]["response_bytes"].as_u64().unwrap_or(0) > 0,
            "ok response is sized"
        );

        audited_call(&server, "shell", json!({}))
            .await
            .expect_err("unknown tool rejected");
        let records = audit_records(&path);
        assert_eq!(records.len(), 2);
        assert!(
            records[1].get("response_bytes").is_none(),
            "protocol errors are unsized"
        );
    }

    #[tokio::test]
    async fn transport_labels_follow_the_session() {
        async fn transport_of(transport: &'static str) -> Value {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("audit.jsonl");
            let mut config = Config::default();
            config.session_user = "testuser".to_owned();
            config.mcp_audit_log = Some(path.clone());
            let registry = Arc::new(register_all());
            let session = McpSession::new_with_transport(config, transport);
            let server = McpServer::new(registry, session);
            audited_call(&server, "whoami", json!({}))
                .await
                .expect("whoami succeeds");
            audit_records(&path).pop().expect("one record")
        }

        assert_eq!(transport_of("stdio").await["transport"], json!("stdio"));
        assert_eq!(transport_of("http").await["transport"], json!("http"));
    }

    /// A down/slow disk must not stall the dispatch worker: every blocking
    /// audit syscall rides `spawn_blocking`.
    ///
    /// The `AUDIT_TEST_DELAY_MS` hook sleeps inside the blocking `open_sink`,
    /// so inline dispatch would park the only worker thread while offloaded
    /// dispatch leaves it free for a concurrent ticker. Single-threaded
    /// runtime on purpose: on a multi-thread pool a parked worker is masked
    /// by its siblings and the test could not fail. The sink filename carries
    /// the `slow-sink` fragment the hook gates on, so concurrent tests on
    /// other temp paths never observe the delay.
    #[tokio::test(flavor = "current_thread")]
    async fn audit_slow_sink_never_stalls_the_worker() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::audit::AUDIT_TEST_DELAY_MS.store(0, Ordering::Relaxed);
            }
        }
        let _reset = Reset;
        crate::audit::AUDIT_TEST_DELAY_MS.store(300, Ordering::Relaxed);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("slow-sink-audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        let server = McpServer::new(registry, session);

        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let ticker = tokio::spawn({
            let ticks = Arc::clone(&ticks);
            async move {
                for _ in 0..100 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    ticks.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        let start = Instant::now();
        audited_call(&server, "whoami", json!({}))
            .await
            .expect("slow sink still answers");
        let ticks_during = ticks.load(Ordering::Relaxed);
        ticker.await.expect("ticker joins");
        let elapsed = start.elapsed();

        // Anti-vacuity: the hook really slept (pre-flight + append).
        assert!(
            elapsed >= Duration::from_millis(500),
            "hook must fire, took only {elapsed:?}"
        );
        assert!(
            ticks_during >= 10,
            "worker stalled on slow sink: only {ticks_during} ticks during {elapsed:?}"
        );
        assert_eq!(audit_records(&path).len(), 1, "record still landed");
    }
}
