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
    ToolDescriptor, ToolRoute, build_tools, dispatch_job_tool, dispatch_tool, job_call_waits,
    job_tool_descriptors, tool_routes,
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
    /// Names advertised with `readOnlyHint: true`, taken from the same
    /// descriptors that build [`tools`](Self::tools) so the classification and
    /// the hint cannot drift. A tool outside this set writes an `intent`
    /// record before it runs.
    read_only_tools: Arc<HashSet<String>>,
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

        // From the post-profile descriptors, so membership is exactly what the
        // surface advertises — the pin `read_only_tools_match_advertised_hints`
        // holds the two together.
        let read_only_tools: HashSet<String> = descriptors
            .iter()
            .filter(|d| d.read_only)
            .map(|d| d.name.clone())
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
            read_only_tools: Arc::new(read_only_tools),
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
        // tools get one only while parked on `wait_seconds`.
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
    /// With `[mcp] audit_log` set and/or the `OTEL_*` endpoint on, a tool the
    /// surface does not advertise `readOnlyHint` writes an `intent` record —
    /// fsynced — *before* it runs, and every call writes an outcome record
    /// (`call`, or `dispatch` for a backgrounded one) afterwards, failures and
    /// unknown tools included. The outcome record carries `intent: <seq>` when
    /// one was written.
    ///
    /// The refusal matrix, in one place:
    ///
    /// * intent write to the file fails → **refuse**; nothing ran, and no
    ///   record exists to say otherwise.
    /// * outcome write to the file fails → warn, append
    ///   [`audit_lost_notice`] to the reply, and return the executed result.
    ///   Discarding it would tell the client a refusal that did not happen.
    /// * either record's export fails → warn only. OTLP is secondary, so
    ///   OTLP-only mode never refuses anything.
    ///
    /// OTLP-only (endpoint set, `audit_log` unset) still builds the JSONL line
    /// in memory and uses it verbatim as the OTLP body. With neither sink this
    /// is dispatch verbatim: behaviour and output are byte-identical.
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
        // Echoed as the record's `trace` only when it parsed.
        let traced = trace.is_some().then_some(traceparent).flatten();
        let auditing = self.session.auditing();
        // The intent and outcome records carry the identical `args`, and
        // building them is not free — a `put` body is hashed — so sanitise once
        // per call rather than once per record.
        let args = auditing.then(|| sanitize_args(name, kwargs));

        // The intent record: a durable "this is about to run", fsynced before
        // it does. Every tool the surface does not advertise `readOnlyHint`
        // gets one, an unknown name included — an unclassified name is not
        // known to be harmless, and one rule beats a second allow-list to keep
        // in step. Read-only tools stay outcome-only, so a poll loop still
        // costs one record per call.
        //
        // A failed intent write is the only refusal left in this function, and
        // the one case where refusing is honest: nothing ran, and no record
        // claims otherwise.
        let mut intent_seq: Option<u64> = None;
        if auditing && !self.read_only_tools.contains(name) {
            let seq = crate::audit::next_seq();
            let mut record = serde_json::json!({
                "v": AUDIT_SCHEMA_VERSION,
                "ts": started_ms,
                "seq": seq,
                "session": self.session.id(),
                "transport": self.session.transport(),
                "event": AuditEvent::Intent.as_str(),
                "tool": crate::audit::cap_str(name),
                "args": args.clone(),
            });
            if let Some(traceparent) = traced {
                record["trace"] = serde_json::json!(traceparent);
            }
            if let Some(audit) = self.session.audit_log()
                && let Err(err) = audit.append_async(record.clone()).await
            {
                return Err(refuse_error(&err));
            }
            self.export_best_effort(&record, name, kwargs, trace);
            intent_seq = Some(seq);
        }

        // Every arm below resolves to one audited outcome; the outcome record
        // is written once at the tail.
        let mut event = AuditEvent::Call;
        let mut rrids: Vec<String> = Vec::new();
        let mut job_ids: Vec<String> = Vec::new();
        let outcome: AuditOutcome;
        let mut result: Result<CallToolResponse, McpError>;

        // A job-control tool: poll/control the session's background-job table.
        if self.job_tools.contains(name) {
            // A `wait_seconds` park holds no lock, so a plain drop on cancel
            // strands nothing; with a `progressToken` it also gets heartbeats.
            // A plain poll, `job_list` and `job_cancel` stay fast and unwrapped —
            // cancelling `job_cancel` makes no sense.
            if !job_call_waits(name, kwargs) {
                let dispatched = dispatch_job_tool(&self.session, name, kwargs, None).await;
                outcome = if dispatched.is_ok() {
                    AuditOutcome::Ok
                } else {
                    AuditOutcome::Error
                };
                result = Ok(render(dispatched).into());
            } else {
                match cancellable(
                    dispatch_job_tool(&self.session, name, kwargs, sink),
                    client_ct,
                )
                .await
                {
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
                        result = Ok(render(dispatched).into());
                    }
                }
            }
        }
        // Acts directly on the loaded checkout. Neither this nor the transfer
        // branch below dispatches through the engine, so neither can hold
        // `/var/lock/mtui.lock`: a plain drop on cancel strands nothing.
        else if self.testreport_tools.contains(name) {
            // Audit-only scope: skipped when no sink is on, so unaudited
            // dispatch never takes the session mutex for the record (#613).
            if auditing {
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
            if auditing {
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

        if auditing {
            // Minted here, after the intent record, so `intent < outcome` holds
            // per call and the file's order tracks `seq`.
            let seq = crate::audit::next_seq();
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
                "args": args,
                "outcome": outcome.as_str(),
                "duration_ms": start.elapsed().as_millis() as u64,
                "rrids": rrids,
                "hosts": hosts,
            });
            if let Some(intent) = intent_seq {
                record["intent"] = serde_json::json!(intent);
            }
            if event == AuditEvent::Dispatch {
                let capped: Vec<String> =
                    job_ids.iter().map(|id| crate::audit::cap_str(id)).collect();
                record["job_ids"] = serde_json::json!(capped);
            }
            if let Some(traceparent) = traced {
                record["trace"] = serde_json::json!(traceparent);
            }
            if let Some(bytes) = response_bytes {
                record["response_bytes"] = serde_json::json!(bytes);
            }
            // File first, seq order, off the worker via `spawn_blocking`. The
            // work is already done, so losing this record can no longer un-run
            // anything: warn, tell the client in band, and return what really
            // happened. A `warn!` alone would leave the client believing a
            // record exists that does not.
            let mut lost: Option<String> = None;
            if let Some(audit) = self.session.audit_log()
                && let Err(err) = audit.append_async(record.clone()).await
            {
                tracing::warn!(
                    seq,
                    tool = %crate::audit::cap_str(name),
                    error = %err,
                    "audit log: outcome record lost"
                );
                lost = Some(err.to_string());
            }
            // OTLP second, same seq and verbatim line.
            self.export_best_effort(&record, name, kwargs, trace);
            if let Some(reason) = lost {
                result = with_audit_notice(result, &reason);
            }
        }
        result
    }

    /// Hand one already-built record to the OTLP exporter, best-effort.
    ///
    /// Never refuses and never reports back: the `mtui.*` attributes are read
    /// back out of `record`, so they cannot disagree with the JSONL body they
    /// accompany. A rejected enqueue warns with a closed-vocabulary reason and
    /// nothing more — `enqueue_audit` has already merged the seq into the gap
    /// it reports on recovery, so accounting it again here would double-count.
    /// The inert encode failure is the one arm that must account for itself.
    fn export_best_effort(
        &self,
        record: &Value,
        name: &str,
        kwargs: &Map<String, Value>,
        trace: Option<([u8; 16], [u8; 8], u8)>,
    ) {
        let Some(otel) = self.session.otel() else {
            return;
        };
        let seq = record["seq"].as_u64().unwrap_or_default();
        let line = serde_json::to_string(record).unwrap_or_default();
        if line.is_empty() {
            otel.note_rejected(seq);
            warn_export_lost(seq, crate::otel::ExportReason::Encode.as_str());
            return;
        }
        let queued = crate::otel::QueuedAudit {
            seq,
            jsonl: line,
            tool: crate::audit::cap_str(name),
            // Absent on an intent record: nothing has happened yet to judge.
            outcome: record["outcome"].as_str().unwrap_or_default().to_owned(),
            event: record["event"].as_str().unwrap_or_default().to_owned(),
            transport: self.session.transport().to_owned(),
            session_id: self.session.id(),
            response_bytes: record["response_bytes"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok()),
            trace,
            time_nanos: crate::otel::now_nanos(),
            extra_attrs: crate::otel::kwarg_otlp_attrs(name, kwargs),
        };
        if let Err(reason) = otel.enqueue_audit(queued) {
            warn_export_lost(seq, enqueue_reason(reason));
        }
    }
}

/// The closed-vocabulary reason for a refused OTLP enqueue: never the
/// endpoint, headers, or URL.
fn enqueue_reason(reason: crate::otel::EnqueueError) -> &'static str {
    match reason {
        crate::otel::EnqueueError::Full => "otlp queue full",
        crate::otel::EnqueueError::Unhealthy => "otlp unhealthy",
    }
}

/// Warn that one record did not reach the exporter.
///
/// The only report an unexported record gets: the call itself still answers.
/// Recovery from a probe-latched exporter does not hang on this line — the
/// refused enqueue already merged its `seq` into the rejected gap, which the
/// flush absorbs and re-POSTs as a gap-only batch every tick. `mtui_mcp::server`
/// is outside `DIAG_EXCLUDED_PREFIXES`, so the line rides the diagnostics queue
/// as a second driver.
fn warn_export_lost(seq: u64, reason: &'static str) {
    tracing::warn!(seq, reason, "audit otlp: record not exported");
}

/// The in-band notice appended to a reply whose outcome record could not be
/// written.
///
/// A `tracing::warn!` never reaches an MCP client, and an executed result must
/// never be thrown away to report a logging failure, so the client is told in
/// the reply itself. `reason` is an `io::Error`'s `Display`, which std builds
/// without the path.
fn audit_lost_notice(reason: &str) -> String {
    format!("[audit: outcome record lost ({reason})]")
}

/// Append [`audit_lost_notice`] to a result without changing its shape.
///
/// Always the **last** content block, so `content[0]` stays the verbatim
/// payload a `--json` tool's caller parses; `is_error` is untouched, and the
/// record's `response_bytes` is taken before this runs, so it sizes the payload
/// rather than the notice.
fn with_audit_notice(
    result: Result<CallToolResponse, McpError>,
    reason: &str,
) -> Result<CallToolResponse, McpError> {
    let notice = audit_lost_notice(reason);
    match result {
        Ok(CallToolResponse::Complete(mut completed)) => {
            completed.content.push(ContentBlock::text(notice));
            Ok(CallToolResponse::Complete(completed))
        }
        Ok(other) => Ok(other),
        Err(mut err) => {
            err.message = format!("{} {notice}", err.message).into();
            Err(err)
        }
    }
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
/// The job-control branch is wrapped only for a `job_status`/`job_result` call
/// parked on `wait_seconds`; a plain poll is fast, and cancelling `job_cancel`
/// makes no sense.
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
    fn read_only_tools_match_advertised_hints() {
        // The intent record is written for every tool this set does not hold,
        // so the classification must be exactly the advertised `readOnlyHint`
        // — a drift either way silently changes what is recorded.
        // Anti-vacuity probes are per profile, since `core` serves neither
        // `whoami` nor `put`.
        for (profile, read_only, mutating) in [
            (
                "full",
                ["list_hosts", "job_status", "whoami", "get"],
                ["run", "job_cancel", "put", "config_set"],
            ),
            (
                "core",
                ["list_hosts", "job_status", "show_log", "testreport_read"],
                ["run", "job_cancel", "update", "testreport_write"],
            ),
        ] {
            let mut config = Config::default();
            config.mcp_profile = profile.to_owned();
            let server = server_with(config);
            for tool in server.tools.iter() {
                let hinted = tool
                    .annotations
                    .as_ref()
                    .and_then(|a| a.read_only_hint)
                    .unwrap_or(false);
                let name = tool.name.to_string();
                assert_eq!(
                    hinted,
                    server.read_only_tools.contains(&name),
                    "{profile}/{name}: classification must follow the advertised hint"
                );
            }
            for name in read_only {
                assert!(
                    server.read_only_tools.contains(name),
                    "{profile}: {name} is read-only"
                );
            }
            for name in mutating {
                assert!(
                    !server.read_only_tools.contains(name),
                    "{profile}: {name} mutates"
                );
            }
        }
    }

    #[test]
    fn with_audit_notice_appends_to_an_error_message() {
        // Not an internal error, so "rebuild it as an internal error" cannot
        // pass: the notice annotates the failure the tool reported, and
        // relabelling a client's own bad request would be a second lie on top
        // of the lost record.
        let original = McpError::invalid_params("boom", None);
        let code = original.code;
        let notified = with_audit_notice(Err(original), "Is a directory (os error 21)")
            .expect_err("an error stays an error");
        assert_eq!(
            notified.message.as_ref(),
            "boom [audit: outcome record lost (Is a directory (os error 21))]"
        );
        assert_eq!(notified.code, code, "the error keeps its own code");
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

    /// `notifications/cancelled` interrupts a parked `job_status` wait: the wait
    /// holds no lock, so the plain `cancellable` drop is the whole recovery.
    #[tokio::test]
    async fn client_cancel_interrupts_a_job_status_wait() {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        /// Never finishes, so only the cancel can end the wait.
        struct Endless;
        #[async_trait::async_trait]
        impl Command for Endless {
            fn name(&self) -> &'static str {
                "endless_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Fanout
            }
            async fn call(
                &self,
                _session: &mut mtui_core::Session,
                _args: &ArgMatches,
            ) -> CommandResult {
                std::future::pending::<()>().await;
                Ok(())
            }
        }

        let session = McpSession::new(Config::default());
        let mut registry = register_all();
        registry.register(Arc::new(Endless));
        let job_id = session
            .start_job(Arc::new(registry), "endless_probe", Vec::new())
            .expect("start_job succeeds");

        let ct = CancellationToken::new();
        let kwargs = serde_json::json!({ "job_id": job_id, "wait_seconds": 60 })
            .as_object()
            .cloned()
            .expect("object");
        let call = tokio::spawn({
            let session = Arc::clone(&session);
            let ct = ct.clone();
            async move {
                cancellable(
                    dispatch_job_tool(&session, "job_status", &kwargs, None),
                    &ct,
                )
                .await
            }
        });
        // The spawned call is the only runnable task, so this parks it on the wait.
        tokio::task::yield_now().await;
        ct.cancel();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), call)
            .await
            .expect("a cancelled wait must return promptly, not run out its budget")
            .expect("spawned task did not panic");
        assert!(outcome.is_none(), "the cancel wins: {outcome:?}");
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

    /// The one record carrying `event`.
    ///
    /// Position is not a contract between a `dispatch` record and its job's
    /// `terminal` one: the worker's append races the dispatch's own, and a job
    /// that settles at once can win. A reader joins by `event` and `job_id`,
    /// so these tests do too.
    fn only_event<'a>(records: &'a [Value], event: &str) -> &'a Value {
        let mut hits = records.iter().filter(|r| r["event"] == json!(event));
        let hit = hits
            .next()
            .unwrap_or_else(|| panic!("no {event:?} record: {records:?}"));
        assert!(
            hits.next().is_none(),
            "exactly one {event:?} record: {records:?}"
        );
        hit
    }

    /// The one captured warning starting with `prefix`, in full.
    ///
    /// Selected by prefix rather than by position so an unrelated event cannot
    /// shift the pin, and required to be unique so a duplicated warning (the
    /// double-accounting shape) cannot pass.
    fn warn_line(logs: &str, prefix: &str) -> String {
        let mut hits = logs.lines().filter(|l| l.starts_with(prefix));
        let line = hits
            .next()
            .unwrap_or_else(|| panic!("no {prefix:?} warning was captured, got: {logs:?}"));
        assert!(
            hits.next().is_none(),
            "exactly one {prefix:?} warning: {logs:?}"
        );
        line.to_owned()
    }

    /// The crate's one capture: `#[tokio::test]` is single-threaded, so the
    /// dispatch's own events land on this thread.
    use crate::test_log::capture_logs;

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
        assert_eq!(records.len(), 2, "a failing call still writes intent first");
        assert_eq!(records[0]["event"], json!("intent"));
        assert_eq!(records[1]["tool"], json!("config_set"));
        assert_eq!(records[1]["outcome"], json!("error"));
        assert_eq!(records[1]["intent"], records[0]["seq"]);
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

        // An unknown name is not known to be read-only, so it takes the same
        // intent path every unclassified tool does.
        let records = audit_records(&path);
        assert_eq!(records.len(), 2, "a refused call produces both records");
        assert_eq!(records[0]["event"], json!("intent"));
        assert_eq!(records[0]["tool"], json!("shell"));
        assert_eq!(records[1]["tool"], json!("shell"));
        assert_eq!(records[1]["outcome"], json!("unknown-tool"));
        assert_eq!(records[1]["intent"], records[0]["seq"]);
        assert_eq!(records[1]["rrids"], json!(Value::Array(vec![])));
        assert_eq!(records[1]["hosts"], json!(Value::Array(vec![])));
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
        let records = audit_records(&path);
        assert_eq!(records.len(), 2, "intent and outcome both redact");
        for record in &records {
            assert_eq!(record["args"]["attribute"], json!("gitea_token"));
            assert_eq!(record["args"]["value"], json!("<redacted>"));
            assert_eq!(record["args"]["secret"], json!(true));
        }
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

        let records = await_records(&path, 3).await;
        assert_eq!(
            records.len(),
            3,
            "intent + dispatch + terminal, nothing else"
        );
        let intent = only_event(&records, "intent");
        assert_eq!(intent["tool"], json!("run"));
        let dispatch = only_event(&records, "dispatch");
        assert_eq!(dispatch["tool"], json!("run"));
        assert!(
            intent["seq"].as_u64() < dispatch["seq"].as_u64(),
            "the intent is minted first: {records:?}"
        );
        assert_eq!(dispatch["outcome"], json!("ok"));
        assert_eq!(dispatch["job_ids"], json!([job_id]));
        assert_eq!(dispatch["rrids"], json!([AUDIT_RRID]));
        assert_eq!(dispatch["hosts"], json!([AUDIT_HOST]));
        assert_eq!(dispatch["intent"], intent["seq"]);
        let terminal = only_event(&records, "terminal");
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
        assert!(
            terminal.get("intent").is_none(),
            "a terminal record joins by job_id, not by intent: {terminal}"
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

        let records = await_records(&path, 3).await;
        assert_eq!(
            records.len(),
            3,
            "intent + dispatch + terminal, nothing else"
        );
        let intent = only_event(&records, "intent");
        let dispatch = only_event(&records, "dispatch");
        assert_eq!(dispatch["intent"], intent["seq"]);
        assert_eq!(dispatch["job_ids"], json!([job_id]));
        let terminal = only_event(&records, "terminal");
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
        // Five records: the run's intent and dispatch, the cancel's intent, the
        // run's terminal — written by the cancel path, which owns it once
        // claimed, so it precedes the cancel call's own record in the file —
        // and the job_cancel outcome. `ts` still orders them causally.
        let records = await_records(&path, 5).await;
        assert_eq!(records.len(), 5, "no duplicate terminal record");
        assert_eq!(records[0]["event"], json!("intent"));
        assert_eq!(records[0]["tool"], json!("run"));
        assert_eq!(records[1]["event"], json!("dispatch"));
        assert_eq!(records[1]["intent"], records[0]["seq"]);
        assert_eq!(records[2]["event"], json!("intent"));
        assert_eq!(records[2]["tool"], json!("job_cancel"));
        assert_eq!(records[4]["tool"], json!("job_cancel"));
        assert_eq!(records[4]["outcome"], json!("ok"));
        assert_eq!(records[4]["intent"], records[2]["seq"]);
        let terminal = &records[3];
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
        // A directory as the sink path: every open fails, so the intent write
        // refuses the call before anything runs.
        config.mcp_audit_log = Some(dir.path().to_path_buf());
        let registry = Arc::new(register_all());
        let session = McpSession::new(config);
        let server = McpServer::new(registry, session.clone());

        // The refusal is the intent write failing, so it only applies to a
        // tool that is not advertised read-only.
        assert!(!server.read_only_tools.contains("run"), "run mutates");
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
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("sink dir").count(),
            0,
            "a refused call leaves no record: there is nowhere to put one"
        );
    }

    #[tokio::test]
    async fn audit_read_only_call_runs_with_an_unwritable_sink() {
        // A read-only tool writes no intent record, so an unwritable sink can
        // only cost its outcome record — never the call.
        async fn whoami(audit: Option<std::path::PathBuf>) -> (CallToolResult, String) {
            let mut config = Config::default();
            config.session_user = "testuser".to_owned();
            config.mcp_audit_log = audit;
            let registry = Arc::new(register_all());
            let session = McpSession::new(config);
            let server = McpServer::new(registry, session);
            assert!(server.read_only_tools.contains("whoami"));
            let (response, logs) = capture_logs(audited_call(&server, "whoami", json!({}))).await;
            let response = response.expect("a read-only call is never refused for the sink");
            (complete_result(&response).clone(), logs)
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let (plain, _) = whoami(None).await;
        let (logged, logs) = whoami(Some(dir.path().to_path_buf())).await;

        assert_eq!(logged.content.len(), 2, "payload plus the notice");
        assert_eq!(
            logged.content[0], plain.content[0],
            "the payload block stays byte-identical to the unaudited reply"
        );
        let notice = logged.content[1]
            .as_text()
            .expect("the notice is a text block")
            .text
            .to_string();
        assert!(
            notice.starts_with("[audit: outcome record lost ("),
            "got: {notice}"
        );
        assert!(notice.ends_with(")]"), "got: {notice}");
        assert!(
            !notice.contains(dir.path().to_str().expect("utf-8 tempdir")),
            "the notice carries no path: {notice}"
        );
        assert!(
            !logs.contains(dir.path().to_str().expect("utf-8 tempdir")),
            "neither does the warning: {logs}"
        );
    }

    /// Breaks the sink from inside the dispatch: moves the live sink aside and
    /// plants a directory in its place, so the outcome append fails on the real
    /// I/O path between the intent record and the outcome one — with no test
    /// hook in production code.
    struct SinkBreaker {
        path: std::path::PathBuf,
        moved: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl mtui_core::Command for SinkBreaker {
        fn name(&self) -> &'static str {
            "sink_breaker"
        }
        fn scope(&self) -> mtui_core::Scope {
            mtui_core::Scope::Fanout
        }
        async fn call(
            &self,
            _session: &mut mtui_core::Session,
            _args: &clap::ArgMatches,
        ) -> mtui_core::CommandResult {
            std::fs::rename(&self.path, &self.moved).expect("move the sink aside");
            std::fs::create_dir(&self.path).expect("plant a directory in its place");
            Ok(())
        }
    }

    #[tokio::test]
    async fn audit_outcome_write_failure_returns_the_result_with_a_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let moved = dir.path().join("audit.moved.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let mut registry = register_all();
        registry.register(Arc::new(SinkBreaker {
            path: path.clone(),
            moved: moved.clone(),
        }));
        let session = McpSession::new(config);
        let server = McpServer::new(Arc::new(registry), session);

        let (response, logs) = capture_logs(audited_call(&server, "sink_breaker", json!({}))).await;
        let response = response.expect("an executed call is never reported as refused");
        let content = &complete_result(&response).content;
        assert_eq!(content.len(), 2, "payload plus the notice");
        let notice = content[1]
            .as_text()
            .expect("the notice is a text block")
            .text
            .to_string();
        assert!(
            notice.starts_with("[audit: outcome record lost ("),
            "got: {notice}"
        );
        assert!(notice.ends_with(")]"), "got: {notice}");
        let dir_path = dir.path().to_str().expect("utf-8 tempdir");
        assert!(
            !notice.contains(dir_path),
            "the notice carries no path: {notice}"
        );

        let intent = audit_records(&moved);
        assert_eq!(intent.len(), 1, "the intent record landed before dispatch");
        assert_eq!(intent[0]["event"], json!("intent"));
        assert_eq!(intent[0]["tool"], json!("sink_breaker"));
        assert!(path.is_dir(), "the breaker really broke the sink");

        // Pinned whole around the two varying tokens: the process-global `seq`
        // and the platform's own errno text.
        let line = warn_line(&logs, "audit log: outcome record lost");
        let (head, _reason) = line
            .split_once(" error=")
            .unwrap_or_else(|| panic!("warn line shape: {line:?}"));
        let seq = head
            .strip_prefix("audit log: outcome record lost seq=")
            .and_then(|rest| rest.strip_suffix(" tool=sink_breaker"))
            .unwrap_or_else(|| panic!("warn line shape: {line:?}"));
        assert!(seq.parse::<u64>().is_ok(), "seq is the join key: {seq:?}");
        assert!(
            !line.contains(dir_path),
            "the warning carries no path: {line}"
        );
    }

    #[tokio::test]
    async fn audit_intent_and_outcome_pair_by_seq() {
        let (server, _session, _dir, path) = audited_server();
        audited_call(
            &server,
            "config_set",
            json!({"attribute": "gitea_token", "value": "pair-secret-4b2c"}),
        )
        .await
        .expect("config_set answers");

        let records = audit_records(&path);
        assert_eq!(
            records.len(),
            2,
            "a mutating call writes intent then outcome"
        );
        let (intent, call) = (&records[0], &records[1]);
        assert_eq!(intent["event"], json!("intent"));
        assert_eq!(call["event"], json!("call"));
        assert_eq!(call["intent"], intent["seq"], "the pair joins by seq");
        assert!(
            intent["seq"].as_u64() < call["seq"].as_u64(),
            "intent precedes its outcome: {intent} / {call}"
        );
        assert_eq!(intent["ts"], call["ts"], "both stamp the call's arrival");
        assert_eq!(intent["tool"], json!("config_set"));
        assert_eq!(intent["session"], call["session"]);
        assert_eq!(intent["args"]["value"], json!("<redacted>"));
        assert_eq!(call["args"]["value"], json!("<redacted>"));
        for absent in ["outcome", "duration_ms", "response_bytes", "rrids", "hosts"] {
            assert!(
                intent.get(absent).is_none(),
                "an intent record carries no {absent}: {intent}"
            );
        }

        // A read-only tool writes the outcome record alone, with no `intent`.
        audited_call(&server, "list_hosts", json!({}))
            .await
            .expect("list_hosts succeeds");
        let records = audit_records(&path);
        assert_eq!(records.len(), 3, "one more record, not two");
        assert_eq!(records[2]["event"], json!("call"));
        assert_eq!(records[2]["tool"], json!("list_hosts"));
        assert!(
            records[2].get("intent").is_none(),
            "a read-only call has no intent record to point at: {}",
            records[2]
        );
    }

    /// Register a never-finishing command and start it as a background job, so
    /// a `job_status` wait has something to park on.
    fn start_endless_job(session: &Arc<McpSession>) -> String {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        struct EndlessAuditProbe;
        #[async_trait::async_trait]
        impl Command for EndlessAuditProbe {
            fn name(&self) -> &'static str {
                "endless_audit_probe"
            }
            // Session-level, so the worker dispatches on a fork and does not
            // hold the canonical session mutex for the life of the job — which
            // the outcome record's `audit_hosts` needs to take.
            fn scope(&self) -> Scope {
                Scope::Single
            }
            fn reads_resolved_report(&self) -> bool {
                false
            }
            async fn call(
                &self,
                _session: &mut mtui_core::Session,
                _args: &ArgMatches,
            ) -> CommandResult {
                std::future::pending::<()>().await;
                Ok(())
            }
        }

        let mut registry = register_all();
        registry.register(Arc::new(EndlessAuditProbe));
        session
            .start_job(Arc::new(registry), "endless_audit_probe", Vec::new())
            .expect("start_job succeeds")
    }

    #[tokio::test]
    async fn audit_plain_job_poll_writes_one_outcome_record() {
        // The job branch answers a plain poll without the cancellable wrapper.
        // It must still fall through to the record rather than returning early,
        // or a poll loop would be the one dispatch path with no audit trail.
        let (server, session, _dir, path) = audited_server();
        assert!(
            server.read_only_tools.contains("job_status"),
            "job_status is advertised read-only, which is what makes one record right"
        );
        let job_id = start_endless_job(&session);

        audited_call(&server, "job_status", json!({ "job_id": job_id }))
            .await
            .expect("a plain poll answers");

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "one record per poll: {records:?}");
        assert_eq!(records[0]["event"], json!("call"));
        assert_eq!(records[0]["tool"], json!("job_status"));
        assert_eq!(records[0]["outcome"], json!("ok"));
        assert!(
            records[0].get("intent").is_none(),
            "a read-only tool writes no intent record: {}",
            records[0]
        );
    }

    #[tokio::test]
    async fn audit_cancelled_job_wait_still_writes_its_outcome_record() {
        // A client-cancelled park is the one arm that answers with an error it
        // did not get from the tool. It is still a call that happened, so it is
        // still recorded — and still without an intent, since `job_status` is
        // read-only however long it parked.
        let (server, session, _dir, path) = audited_server();
        let job_id = start_endless_job(&session);
        let ct = CancellationToken::new();
        let kwargs = json!({ "job_id": job_id, "wait_seconds": 60 });

        let dispatch = server.dispatch_audited(
            "job_status",
            kwargs.as_object().expect("kwargs object"),
            None,
            &ct,
            None,
        );
        let cancel = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ct.cancel();
        };
        let (result, ()) = tokio::join!(dispatch, cancel);
        result.expect_err("the cancel wins the park");

        let records = audit_records(&path);
        assert_eq!(
            records.len(),
            1,
            "one record for the cancelled wait: {records:?}"
        );
        assert_eq!(records[0]["event"], json!("call"));
        assert_eq!(records[0]["tool"], json!("job_status"));
        assert_eq!(records[0]["outcome"], json!("error"));
        assert!(
            records[0].get("intent").is_none(),
            "still read-only: {}",
            records[0]
        );
    }
    /// Register a command forced onto the **exclusive** dispatch path
    /// (`requires_canonical_session`, exactly as `load_template` does) that
    /// parks in its body, and start it as a background job. The worker then
    /// holds the canonical session mutex for the job's whole life — the hold
    /// an audit record must never wait on (#613).
    ///
    /// Returns the job id, the entry signal, and the release handle.
    fn start_parked_exclusive_job(
        session: &Arc<McpSession>,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        struct ExclusivePark {
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }
        #[async_trait::async_trait]
        impl Command for ExclusivePark {
            fn name(&self) -> &'static str {
                "exclusive_park_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Single
            }
            fn reads_resolved_report(&self) -> bool {
                false
            }
            fn requires_canonical_session(&self, _argv: &[String]) -> bool {
                true
            }
            async fn call(
                &self,
                _session: &mut mtui_core::Session,
                _args: &ArgMatches,
            ) -> CommandResult {
                if let Some(tx) = self.entered.lock().expect("probe poisoned").take() {
                    let _ = tx.send(());
                }
                let release = self
                    .release
                    .lock()
                    .expect("probe poisoned")
                    .take()
                    .expect("single dispatch");
                let _ = release.await;
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let mut registry = register_all();
        registry.register(Arc::new(ExclusivePark {
            entered: std::sync::Mutex::new(Some(entered_tx)),
            release: std::sync::Mutex::new(Some(release_rx)),
        }));
        let job_id = session
            .start_job(Arc::new(registry), "exclusive_park_probe", Vec::new())
            .expect("start_job succeeds");
        (job_id, entered_rx, release_tx)
    }

    #[tokio::test]
    async fn audit_outcome_record_never_waits_on_a_parked_exclusive_job() {
        // #613's contention class: the exclusive dispatch holds the canonical
        // session mutex for the whole command, so a record that takes it makes
        // every audited call — a poll of that very job included — wait for the
        // job to finish.
        let (server, session, _dir, path) = audited_server();
        let (job_id, entered, release) = start_parked_exclusive_job(&session);
        tokio::time::timeout(std::time::Duration::from_secs(5), entered)
            .await
            .expect("the probe must reach its body")
            .expect("the probe must signal entry");

        // The exclusive arm's signature, and what makes this test non-vacuous:
        // a shared/scoped dispatch would leave the mutex free and the record
        // would never have waited.
        assert!(
            session.session().try_lock().is_err(),
            "the probe must dispatch on the exclusive path, holding the session"
        );

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            audited_call(&server, "job_status", json!({ "job_id": job_id })),
        )
        .await
        .expect("a poll of a running exclusive job must not wait on the job")
        .expect("job_status answers");
        assert!(
            response_text(&response).contains("running"),
            "the poll sees the job still running: {}",
            response_text(&response)
        );

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "one record per poll: {records:?}");
        assert_eq!(records[0]["tool"], json!("job_status"));
        assert_eq!(records[0]["outcome"], json!("ok"));
        assert_eq!(
            records[0]["hosts"],
            json!([]),
            "no scope resolves for a job tool, so no host names: {}",
            records[0]
        );

        let _ = release.send(());
        let records = await_records(&path, 2).await;
        let terminal = only_event(&records, "terminal");
        assert_eq!(terminal["job_id"], json!(job_id));
        assert_eq!(terminal["job_state"], json!("done"));
    }

    #[tokio::test]
    async fn audit_scope_helpers_degrade_rather_than_wait() {
        // The non-empty-`rrids` arm, which no tool call can reach while the
        // exclusive gate is held: a *second* background job's terminal record
        // resolves its own host names after releasing the gate, by which time
        // another exclusive job may hold the session mutex.
        let (_server, session, _dir, _path) = audited_server();
        seed_audit_host(&session, MockConnection::new(AUDIT_HOST)).await;
        let (_job_id, entered, release) = start_parked_exclusive_job(&session);
        tokio::time::timeout(std::time::Duration::from_secs(5), entered)
            .await
            .expect("the probe must reach its body")
            .expect("the probe must signal entry");
        assert!(
            session.session().try_lock().is_err(),
            "the probe must hold the session"
        );

        let rrids = vec![AUDIT_RRID.to_owned()];
        let held = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            (
                session.audit_hosts(&rrids).await,
                session.audit_template_scope(None).await,
                session.audit_template_scope(Some(AUDIT_RRID)).await,
            )
        })
        .await
        .expect("the audit helpers must not wait on a running job");
        assert_eq!(held.0, Vec::<String>::new(), "host names are best-effort");
        assert_eq!(held.1, Vec::<String>::new(), "so is the implied scope");
        assert_eq!(
            held.2,
            vec![AUDIT_RRID.to_owned()],
            "an explicit template never needed the session at all"
        );

        // The fixture can express the difference: released, the same calls
        // answer with the real host and the real active template.
        let _ = release.send(());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if session.session().try_lock().is_ok() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the job releases the session");
        assert_eq!(
            session.audit_hosts(&rrids).await,
            vec![AUDIT_HOST.to_owned()]
        );
        assert_eq!(
            session.audit_template_scope(None).await,
            vec![AUDIT_RRID.to_owned()]
        );
    }

    #[tokio::test]
    async fn audit_unset_sink_is_byte_identical() {
        async fn call(
            tool: &str,
            kwargs: Value,
            audit: Option<std::path::PathBuf>,
        ) -> CallToolResult {
            let mut config = Config::default();
            config.session_user = "testuser".to_owned();
            config.mcp_audit_log = audit;
            let registry = Arc::new(register_all());
            let session = McpSession::new(config);
            let server = McpServer::new(registry, session);
            let response = audited_call(&server, tool, kwargs)
                .await
                .expect("the call answers");
            complete_result(&response).clone()
        }

        let dir = tempfile::tempdir().expect("tempdir");
        for (tool, kwargs) in [
            ("whoami", json!({})),
            // A mutating tool takes the intent path too, which must stay just
            // as invisible on the wire.
            (
                "config_set",
                json!({"attribute": "session_user", "value": "audited"}),
            ),
        ] {
            let plain = call(tool, kwargs.clone(), None).await;
            let logged = call(
                tool,
                kwargs,
                Some(dir.path().join(format!("{tool}-audit.jsonl"))),
            )
            .await;
            assert_eq!(
                plain, logged,
                "enabling the sink must not change the wire response for {tool}"
            );
        }
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
    async fn unhealthy_otlp_never_gates_dispatch() {
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

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session.clone());

        let lost_before = exporter.audit_lost();
        let (response, logs) = capture_logs(audited_call(&server, "whoami", json!({}))).await;
        let response = response.expect("a collector outage never gates a tool call");
        let text = response_text(&response);
        assert!(text.contains("testuser"), "the call really ran: {text}");

        let records = audit_records(&path);
        assert_eq!(records.len(), 1, "the file sink still holds the record");
        let seq = records[0]["seq"].as_u64().expect("seq in file");
        assert_eq!(
            exporter.audit_lost(),
            lost_before + 1,
            "the refused enqueue is gap-accounted exactly once"
        );
        assert_eq!(
            warn_line(&logs, "audit otlp:"),
            format!("audit otlp: record not exported seq={seq} reason=\"otlp unhealthy\"")
        );
        assert!(!logs.contains("127.0.0.1"), "no endpoint leaks: {logs}");
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn unhealthy_otlp_never_gates_the_intent_record_either() {
        // A mutating tool exports twice, and the intent export is the one that
        // runs *before* dispatch — the one place an export failure could still
        // be turned back into a refusal. It must not be: two refused enqueues,
        // two warnings, two accounted seqs, and the call still runs.
        let failing = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&failing)
            .await;
        let exporter = otlp_exporter(&format!("{}/v1/logs", failing.uri()));
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

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_audit_log = Some(path.clone());
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);
        assert!(
            !server.read_only_tools.contains("config_set"),
            "config_set mutates, so it writes an intent record"
        );

        let lost_before = exporter.audit_lost();
        let (response, logs) = capture_logs(audited_call(
            &server,
            "config_set",
            json!({"attribute": "gitea_token", "value": "intent-export-secret"}),
        ))
        .await;
        response.expect("a collector outage never gates a mutating call either");

        let records = audit_records(&path);
        assert_eq!(records.len(), 2, "intent + outcome: {records:?}");
        assert_eq!(records[0]["event"], json!("intent"));
        assert_eq!(records[1]["event"], json!("call"));
        assert_eq!(
            exporter.audit_lost(),
            lost_before + 2,
            "both refused enqueues are gap-accounted, once each"
        );
        let warns: Vec<&str> = logs
            .lines()
            .filter(|line| line.starts_with("audit otlp:"))
            .collect();
        assert_eq!(
            warns,
            vec![
                format!(
                    "audit otlp: record not exported seq={} reason=\"otlp unhealthy\"",
                    records[0]["seq"]
                ),
                format!(
                    "audit otlp: record not exported seq={} reason=\"otlp unhealthy\"",
                    records[1]["seq"]
                ),
            ],
            "one warning per record, in seq order: {logs:?}"
        );
        assert!(!logs.contains("127.0.0.1"), "no endpoint leaks: {logs}");
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
    async fn otlp_queue_full_never_discards_the_result() {
        // Mirror of the unhealthy case: a filled queue warns through the same
        // closed-vocabulary path and still answers with the executed result.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("test client");
        // No flush loop: nothing ever drains the queue this test fills, and
        // nothing ever POSTs, so the rejection is deterministically `Full`
        // rather than depending on whether the runtime parked between the fill
        // and the call.
        let exporter = crate::otel::OtelExporter::with_client_no_flush(
            crate::otel::OtelConfig::for_tests("http://127.0.0.1:9/v1/logs", "mtui"),
            client,
        );
        // OTLP-only on purpose: with no file sink the record's only path is the
        // exporter, so the refused enqueue is the one accounted loss.
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let registry = Arc::new(register_all());
        let session = McpSession::new_with_otel(config, "stdio", Some(exporter.clone()));
        let server = McpServer::new(registry, session);
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
        assert!(
            exporter.is_healthy(),
            "nothing posted, so the latch is untouched and the refusal is `Full`"
        );

        let lost_before = exporter.audit_lost();
        let (response, logs) = capture_logs(audited_call(&server, "whoami", json!({}))).await;
        let response = response.expect("a full export queue never discards the result");
        let text = response_text(&response);
        assert!(text.contains("testuser"), "the call really ran: {text}");
        assert!(
            !text.contains("[audit:"),
            "export loss is not reported in band: {text}"
        );

        assert_eq!(
            exporter.audit_lost(),
            lost_before + 1,
            "the refused enqueue is gap-accounted exactly once"
        );
        // The line is pinned whole around the one varying token: `seq` is a
        // process-global counter and this mode writes no file to read it from.
        let line = warn_line(&logs, "audit otlp:");
        let (head, reason) = line
            .rsplit_once(" reason=")
            .unwrap_or_else(|| panic!("warn line shape: {line:?}"));
        assert_eq!(reason, "\"otlp queue full\"");
        let seq = head
            .strip_prefix("audit otlp: record not exported seq=")
            .unwrap_or_else(|| panic!("warn line shape: {line:?}"));
        assert!(seq.parse::<u64>().is_ok(), "seq is the join key: {seq:?}");
        assert!(!logs.contains("127.0.0.1"), "no endpoint leaks: {logs}");
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
        let records = audit_records(&path);
        assert_eq!(records.len(), 2, "put mutates: intent then outcome");
        for record in &records {
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
        let traceparent = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ca902b7-01";

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
        assert_eq!(records.len(), 3, "whoami's record, then shell's pair");
        assert!(
            records[1].get("response_bytes").is_none(),
            "an intent record is unsized: nothing has answered yet"
        );
        assert!(
            records[2].get("response_bytes").is_none(),
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
        // A mutating tool, so the two blocking opens under test are the intent
        // append and the outcome append.
        audited_call(
            &server,
            "config_set",
            json!({"attribute": "session_user", "value": "slow"}),
        )
        .await
        .expect("slow sink still answers");
        let ticks_during = ticks.load(Ordering::Relaxed);
        ticker.await.expect("ticker joins");
        let elapsed = start.elapsed();

        // Anti-vacuity: the hook really slept, once per append.
        assert!(
            elapsed >= Duration::from_millis(600),
            "hook must fire twice, took only {elapsed:?}"
        );
        assert!(
            ticks_during >= 10,
            "worker stalled on slow sink: only {ticks_during} ticks during {elapsed:?}"
        );
        assert_eq!(audit_records(&path).len(), 2, "both records still landed");
    }
}
