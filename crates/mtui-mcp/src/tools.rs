//! Synthesise MCP tools from the command [`Registry`].
//!
//! For every command in the registry that is not on the [`crate::deny`] list,
//! this module builds one plain-data [`ToolDescriptor`] whose:
//!
//! * **name** is the command name (e.g. `run`);
//! * **description** is the command's [`about`](mtui_core::Command::about);
//! * **`input_schema`** is derived from the command's built `clap` parser via
//!   `crate::schema::command_input_schema`;
//! * **`read_only`** hint is set conservatively from a name allow-list.
//!
//! The subparser command (`config` today) is fanned out into one tool per
//! subcommand; the bare `config` tool is not emitted, because a "show or set"
//! union schema would mislead the client about which fields are required. Slow
//! host commands gain a `background` boolean.
//!
//! This layer is intentionally **transport-free**: it returns plain descriptors
//! and routes, not `rmcp` types. [`crate::server`] converts a [`ToolDescriptor`]
//! into an `rmcp::model::Tool` and wires `dispatch_tool` into the `ServerHandler`.
//!
//! The background-job path — `dispatch_tool` with `background = true`, plus the
//! four tools from [`job_tool_descriptors`] — drives the session's `_jobs` table:
//! the slow call fans out one job per resolved template and returns their ids
//! immediately, and the job tools poll/control that table.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use mtui_core::{Registry, command_parser};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::deny::is_denied;
use crate::schema::command_input_schema;
use crate::session::{
    DEFAULT_PROGRESS_INTERVAL, JOB_WAIT_CAP_SECS, JobView, McpCommandError, McpSession,
    ProgressSink, ToolOutcome, run_with_heartbeat,
};

/// Commands that touch reference hosts and can run for minutes, so they gain a
/// `background` boolean parameter (see [`dispatch_tool`]). An explicit list.
const SLOW_COMMANDS: &[&str] = &[
    "run",
    "update",
    "downgrade",
    "prepare",
    "install",
    "uninstall",
    "set_repo",
    "reboot",
    "regenerate",
    // Both connect to a whole fleet, where a black-hole candidate host has no
    // other cancellable escape hatch.
    "add_host",
    "load_template",
    // `--watch` polls Slack for up to an hour, far past any MCP client timeout;
    // without it the command just posts and returns.
    "request_review",
];

/// The one command whose `clap` subcommands are fanned out into per-subcommand
/// tools. Pinned (not auto-discovered) so the surface is stable and visible.
const SUBPARSER_COMMANDS: &[&str] = &["config"];

/// A command becomes `read_only` if its name starts with one of these prefixes.
const READ_ONLY_PREFIXES: &[&str] = &["list_", "show_"];

/// Exact names that escape the prefix rule but are still side-effect-free.
/// (`reload_products` is intentionally absent — it re-reads from the hosts.)
const READ_ONLY_EXACT: &[&str] = &["whoami", "openqa_overview", "openqa_jobs"];

/// Tool-call keys still accepted, and ignored, after their property left the
/// tool's schema in 26.4 (#597): inert there, and a pinned client must not start
/// failing on a minor release. Keyed by tool name, which equals the command name
/// for every entry (`dispatch_tool` looks up `route.command`). Delete with the
/// 26.5 version bump; `deprecated_kwargs_expire_with_26_5` fails that bump until
/// this is empty. `load_template` is absent on purpose: its `-T` failed the call
/// before it ran, so its keys are refused outright.
const DEPRECATED_KWARGS: &[(&str, &[&str])] = &[
    ("list_refhosts", &["template", "all_templates"]),
    ("list_templates", &["template", "all_templates"]),
    ("regenerate", &["all_templates"]),
    ("set_log_level", &["template", "all_templates"]),
    ("unload", &["template", "all_templates"]),
    ("updates", &["template", "all_templates"]),
    ("whoami", &["template", "all_templates"]),
];

/// The deprecated keys of `tool` (empty for every other tool).
fn deprecated_kwargs(tool: &str) -> &'static [&'static str] {
    match DEPRECATED_KWARGS.iter().find(|(name, _)| *name == tool) {
        Some((_, keys)) => keys,
        None => &[],
    }
}

/// A synthesised MCP tool as plain data; [`crate::server`] converts it into an
/// `rmcp::model::Tool` with its `ToolAnnotations { read_only_hint }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    /// The tool name (command name, or `config_<sub>` for the fan-out).
    pub name: String,
    /// One-line description shown to the client.
    pub description: String,
    /// JSON-Schema `object` for the tool's inputs.
    pub input_schema: Map<String, Value>,
    /// Conservative `readOnlyHint`: `true` only for known side-effect-free tools.
    pub read_only: bool,
}

/// How a tool name routes back to the engine when called, built in the same pass
/// as the descriptors so a tool's schema and its dispatch cannot diverge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRoute {
    /// The registry command name to dispatch (`config` for `config_show`).
    command: &'static str,
    /// Tokens prepended to the reconstructed argv (`["show"]` for `config_show`).
    argv_prefix: Vec<String>,
    /// Whether this tool accepts the `background` flag (a slow host command).
    slow: bool,
}

/// `true` iff a command is known to be side-effect-free.
fn is_read_only(name: &str) -> bool {
    READ_ONLY_EXACT.contains(&name) || READ_ONLY_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Reject any tool-call kwarg not in `allowed`, mirroring the strict
/// `additionalProperties: false` the advertised schema carries.
///
/// The runtime half for clients that do not validate: without it a misspelled
/// field (`temlate=`, `mesage=`) is silently dropped and the command runs with
/// it missing. Keys are reported sorted for a deterministic message.
pub(crate) fn reject_unknown_kwargs<'a>(
    kwargs: &Map<String, Value>,
    allowed: impl IntoIterator<Item = &'a str>,
) -> Result<(), McpCommandError> {
    let allowed: std::collections::BTreeSet<&str> = allowed.into_iter().collect();
    let mut unknown: Vec<&str> = kwargs
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    let names = unknown.join(", ");
    Err(McpCommandError {
        stdout: String::new(),
        stderr: format!("unknown argument(s): {names}"),
        exit_code: 1,
    })
}

/// The `clap` subcommand a fanned-out subparser tool's args live on, resolved
/// through the route's single-element `argv_prefix` so the allowed-arg set
/// matches the advertised schema. `None` for a plain tool.
fn subparser_layer<'a>(
    parser: &'a clap::Command,
    argv_prefix: &[String],
) -> Option<&'a clap::Command> {
    let [sub_name] = argv_prefix else { return None };
    parser.get_subcommands().find(|c| c.get_name() == sub_name)
}

/// Inject a `background` boolean (default false, not required) into a slow
/// command's input schema.
fn add_background_property(schema: &mut Map<String, Value>) {
    let props = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(props) = props {
        props.insert(
            "background".to_owned(),
            json!({
                "type": "boolean",
                "default": false,
                "description": "Return a job id immediately instead of blocking; \
                    job_status/job_result with wait_seconds=N then block for it.",
            }),
        );
    }
}

/// One internal walk that produces both the descriptors and their routes, so the
/// two views can never disagree on the tool set.
fn synthesise(registry: &Registry) -> (Vec<ToolDescriptor>, BTreeMap<String, ToolRoute>) {
    warn_on_deny_drift(registry);

    let mut descriptors: Vec<ToolDescriptor> = Vec::new();
    let mut routes: BTreeMap<String, ToolRoute> = BTreeMap::new();

    let mut names: Vec<&'static str> = registry.names().collect();
    names.sort_unstable();

    for name in names {
        let command = registry
            .get(name)
            .expect("registry.names() yields registered commands");
        if is_denied(name) || command.aliases().iter().any(|alias| is_denied(alias)) {
            continue;
        }

        if SUBPARSER_COMMANDS.contains(&name) {
            fan_out_subparser(command.as_ref(), name, &mut descriptors, &mut routes);
            continue;
        }

        let parser = command_parser(command.as_ref());
        let mut input_schema = command_input_schema(&parser);
        let slow = SLOW_COMMANDS.contains(&name);
        if slow {
            add_background_property(&mut input_schema);
        }
        descriptors.push(ToolDescriptor {
            name: name.to_owned(),
            description: command.about().unwrap_or(name).trim().to_owned(),
            input_schema,
            read_only: is_read_only(name),
        });
        routes.insert(
            name.to_owned(),
            ToolRoute {
                command: name,
                argv_prefix: Vec::new(),
                slow,
            },
        );
    }

    descriptors.sort_by(|a, b| a.name.cmp(&b.name));
    (descriptors, routes)
}

/// Register one tool per subcommand of a subparser command (`config`); the bare
/// parent is not emitted, and `config` is not slow, so no `background`.
fn fan_out_subparser(
    command: &dyn mtui_core::Command,
    name: &'static str,
    descriptors: &mut Vec<ToolDescriptor>,
    routes: &mut BTreeMap<String, ToolRoute>,
) {
    let parser = command_parser(command);
    for sub in parser.get_subcommands() {
        let sub_name = sub.get_name().to_owned();
        let tool_name = format!("{name}_{sub_name}");
        let description = sub
            .get_about()
            .map(|s| s.to_string())
            .unwrap_or_else(|| tool_name.clone());
        descriptors.push(ToolDescriptor {
            name: tool_name.clone(),
            description,
            input_schema: command_input_schema(sub),
            read_only: is_read_only(&tool_name),
        });
        routes.insert(
            tool_name,
            ToolRoute {
                command: name,
                argv_prefix: vec![sub_name],
                slow: false,
            },
        );
    }
}

/// Warn (do not fail) if a deny-listed name is absent from the live registry — a
/// renamed/removed command should surface at boot rather than silently leak.
fn warn_on_deny_drift(registry: &Registry) {
    let missing: Vec<&str> = crate::deny::MCP_DENYLIST
        .iter()
        .copied()
        .filter(|name| !registry.contains(name))
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            missing = ?missing,
            "deny-list entries missing from the command registry; rename or remove \
             the stale entries in mtui_core::MCP_DENYLIST",
        );
    }
}

/// Build the synthesised command-tool descriptors, sorted by name: deny-listed
/// commands skipped, the `config` subparser fanned out, a `background` flag on
/// slow host commands. The job tools are [`job_tool_descriptors`]'s.
#[must_use]
pub fn build_tools(registry: &Registry) -> Vec<ToolDescriptor> {
    synthesise(registry).0
}

/// Build the tool-name → [`ToolRoute`] map for dispatching calls back to the
/// engine. Keys match [`build_tools`] descriptor names exactly.
#[must_use]
pub(crate) fn tool_routes(registry: &Registry) -> BTreeMap<String, ToolRoute> {
    synthesise(registry).1
}

/// Dispatch a synthesised command tool call back through the engine.
///
/// Pops the `background` flag for slow commands; when `true` the call fans out
/// jobs via [`McpSession::start_jobs`], one per resolved template, and returns
/// their ids to poll. Otherwise it reconstructs argv from `kwargs` (honouring
/// the route's `argv_prefix`) and runs it through
/// [`McpSession::run_command_with_progress`] or, with a `client_ct`,
/// [`McpSession::run_command_client_cancellable`] — emitting heartbeats via
/// `sink` so a slow foreground call does not time the client out.
///
/// `client_ct` is the MCP request's own cancellation token: only this
/// synthesised-command path can hold `/var/lock/mtui.lock`, so it is the one
/// call site that needs the two-stage cancel/abort/unlock sequence instead of
/// the bare drop [`crate::server`] uses for the testreport/transfer branches.
pub(crate) async fn dispatch_tool(
    registry: &Arc<Registry>,
    session: &Arc<McpSession>,
    route: &ToolRoute,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
    client_ct: Option<&CancellationToken>,
) -> ToolDispatch {
    let mut kwargs = kwargs.clone();
    let background = if route.slow {
        matches!(kwargs.remove("background"), Some(Value::Bool(true)))
    } else {
        false
    };

    let Some(command) = registry.get(route.command) else {
        return Err::<String, _>(McpCommandError {
            stdout: String::new(),
            stderr: format!("command not registered: {}", route.command),
            exit_code: 1,
        })
        .into();
    };
    let parser = command_parser(command.as_ref());

    // Reject misspelled fields before argv reconstruction silently drops them.
    // The allowed keys are the callable args of the parser layer that produced
    // this tool's schema — for a fanned-out tool (`config_show`) the *subcommand*,
    // where its args live, not the parent. `background` was popped above.
    let arg_source = subparser_layer(&parser, &route.argv_prefix).unwrap_or(&parser);
    // Deprecated keys stay accepted for one release; `kwargs_to_argv` walks the
    // parser's args, so they never reach argv.
    let deprecated = deprecated_kwargs(route.command);
    let sent: Vec<&str> = deprecated
        .iter()
        .copied()
        .filter(|key| kwargs.contains_key(*key))
        .collect();
    if !sent.is_empty() {
        tracing::warn!(
            tool = route.command,
            keys = ?sent,
            "ignoring deprecated tool-call keys: dropped from the schema in 26.4, refused from 26.5 (#597)"
        );
    }
    let allowed = arg_source
        .get_arguments()
        .map(|a| a.get_id().as_str())
        .filter(|id| *id != "help" && *id != "version")
        .chain(deprecated.iter().copied());
    if let Err(err) = reject_unknown_kwargs(&kwargs, allowed) {
        return Err::<String, _>(err).into();
    }

    // The same layer, or reconstruction drops every kwarg the parent does not
    // declare: `config set` emitted a bare `["set"]` and clap rejected it for the
    // missing required positionals, `config show`'s filter vanished.
    let argv = crate::argv::kwargs_to_argv(arg_source, &kwargs, &route.argv_prefix);

    // The template scope this call resolves to, for the audit record. Resolved
    // with the same resolver the lock and background paths use, so the three
    // cannot disagree.
    let rrids = session
        .resolve_job_rrids(registry, route.command, &argv)
        .await
        .unwrap_or_default();

    if background {
        return match session
            .start_jobs(Arc::clone(registry), route.command, argv)
            .await
        {
            Ok(job_ids) => {
                let reply = started_jobs_reply(route.command, &job_ids);
                ToolDispatch {
                    outcome: ToolOutcome::Completed(Ok(reply)),
                    jobs: job_ids,
                    rrids,
                }
            }
            Err(err) => ToolDispatch {
                outcome: ToolOutcome::Completed(Err(err)),
                jobs: Vec::new(),
                rrids,
            },
        };
    }

    let outcome = match client_ct {
        Some(ct) => {
            session
                .run_command_client_cancellable(
                    registry,
                    route.command,
                    &argv,
                    sink,
                    DEFAULT_PROGRESS_INTERVAL,
                    ct,
                )
                .await
        }
        None => session
            .run_command_with_progress(
                registry,
                route.command,
                &argv,
                sink,
                DEFAULT_PROGRESS_INTERVAL,
            )
            .await
            .into(),
    };
    ToolDispatch {
        outcome,
        jobs: Vec::new(),
        rrids,
    }
}

/// What [`dispatch_tool`] ran and what it touched: the engine outcome, the
/// background job ids it minted (empty unless backgrounded), and the template
/// scope its arguments resolved to (empty when the command addresses no
/// template). The server layer records all three in the audit record.
pub(crate) struct ToolDispatch {
    pub outcome: ToolOutcome,
    pub jobs: Vec<String>,
    pub rrids: Vec<String>,
}

impl From<ToolOutcome> for ToolDispatch {
    fn from(outcome: ToolOutcome) -> Self {
        Self {
            outcome,
            jobs: Vec::new(),
            rrids: Vec::new(),
        }
    }
}

impl From<Result<String, McpCommandError>> for ToolDispatch {
    fn from(result: Result<String, McpCommandError>) -> Self {
        ToolOutcome::from(result).into()
    }
}

/// The client-facing reply after starting one or more background jobs: a single
/// job points at `job_status`/`job_result`, a fan-out lists every id.
fn started_jobs_reply(command: &str, job_ids: &[String]) -> String {
    if let [job_id] = job_ids {
        return format!(
            "started job '{job_id}' (`{command}`); \
             job_result('{job_id}', wait_seconds=N) blocks up to N s for its \
             output; job_status('{job_id}', wait_seconds=N) for state only."
        );
    }
    let joined = job_ids
        .iter()
        .map(|j| format!("'{j}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "started {} jobs (`{command}`, one per template): {joined}. \
         job_result(id, wait_seconds=N) blocks up to N s per job; \
         job_status(id, wait_seconds=N) for state.",
        job_ids.len()
    )
}

/// The four background-job control tools, routed onto the session's job table by
/// `dispatch_job_tool`.
///
/// Their names and schemas are a downstream contract and snapshot-tested; the
/// schemas are strict (`additionalProperties: false`), so a misspelled field is a
/// clean error rather than a silently ignored argument.
#[must_use]
pub fn job_tool_descriptors() -> Vec<ToolDescriptor> {
    let job_schema = |wait: bool| {
        let mut props = Map::new();
        props.insert(
            "job_id".to_owned(),
            json!({ "type": "string", "description": "The background job id." }),
        );
        if wait {
            props.insert(
                "wait_seconds".to_owned(),
                json!({
                    "type": "integer",
                    "minimum": 0,
                    "maximum": JOB_WAIT_CAP_SECS,
                    "default": 0,
                    "description": format!(
                        "Block up to this many seconds for the job to finish (max \
                         {JOB_WAIT_CAP_SECS}; keep below the client's request timeout)."
                    ),
                }),
            );
        }
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(props));
        schema.insert("required".to_owned(), json!(["job_id"]));
        schema.insert("additionalProperties".to_owned(), Value::Bool(false));
        schema
    };
    let empty_schema = || {
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(Map::new()));
        schema.insert("additionalProperties".to_owned(), Value::Bool(false));
        schema
    };

    vec![
        ToolDescriptor {
            name: "job_list".to_owned(),
            description: "List background jobs in this session and their state \
                (running/done/failed/cancelled)."
                .to_owned(),
            input_schema: empty_schema(),
            read_only: true,
        },
        ToolDescriptor {
            name: "job_status".to_owned(),
            description: "Report a background job's state and elapsed time. \
                wait_seconds>0 blocks until it finishes or the budget lapses, then \
                reports either way."
                .to_owned(),
            input_schema: job_schema(true),
            read_only: true,
        },
        ToolDescriptor {
            name: "job_result".to_owned(),
            description: "Return a finished background job's output; wait_seconds>0 \
                blocks for it first. Errors if still running after the wait, or \
                surfaces the command's failure."
                .to_owned(),
            input_schema: job_schema(true),
            read_only: true,
        },
        ToolDescriptor {
            name: "job_cancel".to_owned(),
            description: "Cancel a running background job. A job already executing on a \
                host may keep running there even after cancel; the operation lock the \
                job's own host group took is released best-effort (bounded, and never \
                a comment-marked reservation) and the reply reports the outcome."
                .to_owned(),
            input_schema: job_schema(false),
            read_only: false,
        },
    ]
}

/// Whether a job-tool call will park on a `wait_seconds` budget.
///
/// The server's wrap decision only — a malformed or over-cap value still reaches
/// [`dispatch_job_tool`], which refuses it.
pub(crate) fn job_call_waits(name: &str, kwargs: &Map<String, Value>) -> bool {
    matches!(name, "job_status" | "job_result")
        && kwargs
            .get("wait_seconds")
            .and_then(Value::as_i64)
            .is_some_and(|s| s > 0)
}

/// Dispatch a job-control tool call against the session's `_jobs` table.
///
/// Routes each of the four names to the matching [`McpSession`] method and
/// renders its result into the one-line text the client sees.
///
/// # Errors
///
/// Returns [`McpCommandError`] when a `job_id` is missing/unknown, when
/// `wait_seconds` is not an integer in `0..=JOB_WAIT_CAP_SECS`, when
/// `job_result` is polled on a still-running / failed / cancelled job, or when
/// the tool name is unrecognised.
pub(crate) async fn dispatch_job_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
) -> Result<String, McpCommandError> {
    dispatch_job_tool_with_interval(session, name, kwargs, sink, DEFAULT_PROGRESS_INTERVAL).await
}

/// [`dispatch_job_tool`] with an explicit heartbeat interval, so the colocated
/// heartbeat test drives a sub-second one.
///
/// # Errors
///
/// As [`dispatch_job_tool`].
pub(crate) async fn dispatch_job_tool_with_interval(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
    interval: Duration,
) -> Result<String, McpCommandError> {
    // The allowed keys are the descriptor's advertised properties, so this cannot
    // drift from the tool's strict schema.
    if let Some(desc) = job_tool_descriptors().into_iter().find(|d| d.name == name) {
        let allowed = desc
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|props| props.keys().map(String::as_str));
        reject_unknown_kwargs(kwargs, allowed)?;
    }

    match name {
        "job_list" => {
            let jobs = session.job_list();
            if jobs.is_empty() {
                return Ok("no background jobs".to_owned());
            }
            Ok(jobs
                .iter()
                .map(|j| format!("- {}", format_job_view(j)))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "job_status" => {
            let job_id = job_id_arg(kwargs)?;
            let budget = wait_seconds_arg(kwargs)?;
            let view = async {
                Ok(format_job_view(
                    &session.job_status_wait(&job_id, budget).await?,
                ))
            };
            heartbeat_while_parked(view, sink, name, interval).await
        }
        "job_result" => {
            let job_id = job_id_arg(kwargs)?;
            let budget = wait_seconds_arg(kwargs)?;
            let output = session.job_result_wait(&job_id, budget);
            heartbeat_while_parked(output, sink, name, interval).await
        }
        "job_cancel" => {
            let job_id = job_id_arg(kwargs)?;
            session.job_cancel(&job_id).await
        }
        other => Err(McpCommandError {
            stdout: String::new(),
            stderr: format!("unknown job tool: {other}"),
            exit_code: 1,
        }),
    }
}

/// Drive `fut`, heart-beating whenever the caller supplied a sink.
///
/// A zero budget needs no guard of its own. The server hands a sink only to a
/// call [`job_call_waits`] says will park, and `run_with_heartbeat`'s `biased`
/// select returns an already-ready future before its first tick — so a poll
/// emits nothing either way, and a guard here would be untestable.
async fn heartbeat_while_parked<F: Future>(
    fut: F,
    sink: Option<&dyn ProgressSink>,
    name: &str,
    interval: Duration,
) -> F::Output {
    match sink {
        Some(sink) => run_with_heartbeat(fut, sink, name, interval).await,
        None => fut.await,
    }
}

/// Render a [`JobView`] as the one-line `job_status` text; `job_list` prepends
/// `"- "` to each.
fn format_job_view(job: &JobView) -> String {
    format!(
        "{}: {} ({}s) [{}]",
        job.id, job.state, job.elapsed_s, job.command
    )
}

/// Extract the required `job_id` string argument, or a parse-style error.
fn job_id_arg(kwargs: &Map<String, Value>) -> Result<String, McpCommandError> {
    match kwargs.get("job_id").and_then(Value::as_str) {
        Some(id) => Ok(id.to_owned()),
        None => Err(McpCommandError {
            stdout: String::new(),
            stderr: "job_id is required".to_owned(),
            exit_code: 2,
        }),
    }
}

/// Extract the optional `wait_seconds` budget, or a parse-style error.
///
/// Absent or null is zero — the non-blocking poll. Above
/// [`JOB_WAIT_CAP_SECS`] is refused rather than clamped: the schema advertises
/// the maximum, so a larger value is a client bug, and clamping would let it
/// believe it had waited the whole time it asked for.
fn wait_seconds_arg(kwargs: &Map<String, Value>) -> Result<Duration, McpCommandError> {
    let refuse = |stderr: String| McpCommandError {
        stdout: String::new(),
        stderr,
        exit_code: 2,
    };
    match kwargs.get("wait_seconds") {
        None | Some(Value::Null) => Ok(Duration::ZERO),
        Some(Value::Number(n)) => {
            // An integer past `i64::MAX` fits no signed slot but is still an
            // integer: it must hit the cap's refusal, not the shape one.
            let secs = match (n.as_i64(), n.as_u64()) {
                (Some(signed), _) => u64::try_from(signed)
                    .map_err(|_| refuse(format!("wait_seconds must be >= 0 (got {signed})")))?,
                (None, Some(huge)) => huge,
                (None, None) => {
                    return Err(refuse(format!("wait_seconds must be an integer, got {n}")));
                }
            };
            if secs > JOB_WAIT_CAP_SECS {
                return Err(refuse(format!(
                    "wait_seconds must be <= {JOB_WAIT_CAP_SECS} (got {secs})"
                )));
            }
            Ok(Duration::from_secs(secs))
        }
        Some(other) => Err(refuse(format!(
            "wait_seconds must be an integer, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clap::ArgMatches;
    use mtui_config::Config;
    use mtui_core::{Command, CommandResult, Scope, Session, register_all};

    /// Unwraps a [`ToolOutcome`] produced with `client_ct: None`, which can
    /// only ever be [`ToolOutcome::Completed`].
    fn completed(outcome: ToolOutcome) -> Result<String, McpCommandError> {
        match outcome {
            ToolOutcome::Completed(result) => result,
            ToolOutcome::Aborted(_) => panic!("client_ct was None; expected Completed"),
        }
    }

    // ------------------------------------------------------ reject_unknown_kwargs

    #[test]
    fn reject_unknown_kwargs_accepts_only_known_keys() {
        let kwargs = json!({ "template": "a:b:1:1" });
        let kwargs = kwargs.as_object().unwrap();
        reject_unknown_kwargs(kwargs, ["template", "all_templates"]).expect("known key allowed");
    }

    #[test]
    fn reject_unknown_kwargs_empty_is_ok() {
        let kwargs = Map::new();
        reject_unknown_kwargs(&kwargs, ["anything"]).expect("no kwargs is fine");
    }

    #[test]
    fn reject_unknown_kwargs_reports_offenders_sorted() {
        let kwargs = json!({ "zzz": 1, "aaa": 2, "template": "ok" });
        let kwargs = kwargs.as_object().unwrap();
        let err = reject_unknown_kwargs(kwargs, ["template"]).expect_err("typos refused");
        assert_eq!(err.exit_code, 1);
        assert!(err.stdout.is_empty());
        assert_eq!(err.stderr, "unknown argument(s): aaa, zzz");
    }

    struct AliasedCommand;

    #[async_trait]
    impl Command for AliasedCommand {
        fn name(&self) -> &'static str {
            "renamed_shell"
        }

        fn aliases(&self) -> &'static [&'static str] {
            &["shell"]
        }

        fn scope(&self) -> Scope {
            Scope::Single
        }

        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    fn descriptor<'a>(tools: &'a [ToolDescriptor], name: &str) -> &'a ToolDescriptor {
        tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tool {name} not found; have: {:?}", names(tools)))
    }

    fn names(tools: &[ToolDescriptor]) -> Vec<&str> {
        tools.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn deny_listed_commands_are_not_synthesised() {
        let tools = build_tools(&register_all());
        let routes = tool_routes(&register_all());
        for denied in ["quit", "exit", "EOF", "edit", "shell", "help", "switch"] {
            assert!(
                !names(&tools).contains(&denied),
                "denied command {denied} leaked into tools"
            );
            assert!(
                !routes.contains_key(denied),
                "denied command {denied} leaked into routes"
            );
        }
        assert!(names(&tools).contains(&"run"));
        assert!(routes.contains_key("run"));
    }

    #[test]
    fn command_with_denied_alias_is_not_synthesised() {
        let mut registry = Registry::new();
        registry.register(Arc::new(AliasedCommand));

        assert!(build_tools(&registry).is_empty());
        assert!(tool_routes(&registry).is_empty());
    }

    #[test]
    fn config_is_fanned_out_bare_config_absent() {
        let tools = build_tools(&register_all());
        let ns = names(&tools);
        assert!(!ns.contains(&"config"), "bare config must not be a tool");
        assert!(ns.contains(&"config_show"), "config_show missing");
        assert!(ns.contains(&"config_set"), "config_set missing");
    }

    /// A fanned-out tool's description is its subcommand's `about`, so `config
    /// show`/`config set` must carry one or the client sees the bare tool name;
    /// `set`'s positionals are described too (#597).
    #[test]
    fn config_tools_are_described() {
        let tools = build_tools(&register_all());
        let set = descriptor(&tools, "config_set");
        assert_ne!(set.description, "config_set");
        assert!(set.description.starts_with("Sets"), "{:?}", set.description);
        let show = descriptor(&tools, "config_show");
        assert_ne!(show.description, "config_show");
        assert!(
            show.description.starts_with("Shows"),
            "{:?}",
            show.description
        );
        for prop in ["attribute", "value"] {
            assert!(!prop_description(&tools, "config_set", prop).is_empty());
        }
    }

    #[test]
    fn config_set_schema_requires_attribute_and_value() {
        let tools = build_tools(&register_all());
        let set = descriptor(&tools, "config_set");
        let required = set.input_schema.get("required").expect("required present");
        let required: Vec<&str> = required
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"attribute"), "attribute required");
        assert!(required.contains(&"value"), "value required");
    }

    fn prop_names(tools: &[ToolDescriptor], name: &str) -> Vec<String> {
        descriptor(tools, name)
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A tool whose command addresses no template exposes neither `template`
    /// nor `all_templates`; a `Scope::Single` one that reads the report keeps
    /// only `template` (#597).
    #[test]
    fn session_level_tools_declare_neither_template_property() {
        let tools = build_tools(&register_all());
        for name in [
            "load_template",
            "unload",
            "list_templates",
            "list_refhosts",
            "updates",
            "whoami",
            "set_log_level",
        ] {
            let props = prop_names(&tools, name);
            assert!(!props.contains(&"template".to_owned()), "{name}: {props:?}");
            assert!(
                !props.contains(&"all_templates".to_owned()),
                "{name}: {props:?}"
            );
        }
        let regenerate = prop_names(&tools, "regenerate");
        assert!(
            regenerate.contains(&"template".to_owned()),
            "{regenerate:?}"
        );
        assert!(
            !regenerate.contains(&"all_templates".to_owned()),
            "{regenerate:?}"
        );
        let list_hosts = prop_names(&tools, "list_hosts");
        assert!(
            list_hosts.contains(&"template".to_owned()),
            "{list_hosts:?}"
        );
        assert!(
            list_hosts.contains(&"all_templates".to_owned()),
            "{list_hosts:?}"
        );
    }

    fn prop_description<'a>(tools: &'a [ToolDescriptor], name: &str, prop: &str) -> &'a str {
        descriptor(tools, name)
            .input_schema
            .get("properties")
            .and_then(|p| p.get(prop))
            .and_then(|p| p.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{name}.{prop} carries no description"))
    }

    /// The `all_templates` description states each scope's default in one
    /// clause: it rides on every fan-out-capable tool of every `tools/list`
    /// (#597).
    #[test]
    fn all_templates_help_is_one_clause_per_scope() {
        let tools = build_tools(&register_all());
        for (name, expected) in [
            ("list_hosts", "the default for this command"),
            ("update", "never implicitly fans out"),
            ("list_products", "the active one"),
        ] {
            let help = prop_description(&tools, name, "all_templates");
            assert!(help.contains(expected), "{name}: {help:?}");
            assert!(help.chars().count() <= 110, "{name}: {help:?}");
        }
    }

    /// `template=` on `load_template` is an unknown key, refused before argv
    /// reconstruction — not a `TemplateNotLoaded` from the resolver (#597).
    /// Both keys, because `load_template` is the one command whose `-T` failed
    /// the call before it ran: it is exempt from the one-release shim the other
    /// session-level tools get, so neither key may be listed for it.
    #[tokio::test]
    async fn load_template_with_a_template_kwarg_is_refused_as_unknown() {
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("load_template").expect("load_template route");
        let kwargs = json!({
            "auto": "SUSE:Maintenance:2:2",
            "template": "SUSE:Maintenance:2:2",
            "all_templates": true,
        });
        let err = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect_err("template kwargs refused");
        assert_eq!(err.stderr, "unknown argument(s): all_templates, template");
        assert_eq!(err.exit_code, 1);
    }

    /// The `template`/`all_templates` keys dropped from these tools' schemas in
    /// 26.4 stay accepted and inert for one release: identical output to
    /// omitting them, and the unloaded RRID is never resolved (#597).
    /// `set_log_level` is deliberately not exercised here — its body mutates the
    /// process-wide log filter, which this crate's tests share.
    #[tokio::test]
    async fn deprecated_template_kwargs_are_ignored_on_session_level_tools() {
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        for (tool, expected) in [
            ("whoami", "User: "),
            ("list_templates", "no templates loaded"),
        ] {
            let session = McpSession::new(Config::default());
            let route = routes.get(tool).unwrap_or_else(|| panic!("{tool} route"));
            let mut outputs = Vec::new();
            for kwargs in [
                json!({}),
                json!({ "template": "SUSE:Maintenance:9:9" }),
                json!({ "all_templates": true }),
            ] {
                let out = completed(
                    dispatch_tool(
                        &registry,
                        &session,
                        route,
                        kwargs.as_object().unwrap(),
                        None,
                        None,
                    )
                    .await
                    .outcome,
                )
                .unwrap_or_else(|e| panic!("{tool} with {kwargs}: {e}"));
                assert!(out.contains(expected), "{tool} with {kwargs}: {out:?}");
                outputs.push(out);
            }
            assert_eq!(
                outputs[0], outputs[1],
                "{tool}: template= changed the output"
            );
            assert_eq!(
                outputs[0], outputs[2],
                "{tool}: all_templates= changed the output"
            );
        }
    }

    /// `regenerate` kept `-T` but lost `--all-templates`, so only that key is
    /// shimmed: the call runs and fails on its own terms, not on the key (#597).
    #[tokio::test]
    async fn regenerate_all_templates_kwarg_is_ignored() {
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("regenerate").expect("regenerate route");
        let mut errors = Vec::new();
        for kwargs in [json!({}), json!({ "all_templates": true })] {
            let err = completed(
                dispatch_tool(
                    &registry,
                    &session,
                    route,
                    kwargs.as_object().unwrap(),
                    None,
                    None,
                )
                .await
                .outcome,
            )
            .expect_err("nothing loaded, so regenerate fails either way");
            assert!(
                !err.stderr.contains("unknown argument"),
                "{kwargs}: {:?}",
                err.stderr
            );
            errors.push(err);
        }
        assert_eq!(errors[0].stderr, errors[1].stderr);
        assert_eq!(errors[0].exit_code, errors[1].exit_code);
        assert!(
            errors[0].stderr.contains("Metadata not loaded"),
            "{:?}",
            errors[0].stderr
        );
    }

    /// The shim exempts named keys, not unknown keys at large: a typo on a
    /// shimmed tool is still refused. Green before the shim landed too — its job
    /// is to stay green after it (#597).
    #[tokio::test]
    async fn typo_keys_are_still_refused_on_a_shimmed_tool() {
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("whoami").expect("whoami route");
        let kwargs = json!({ "temlate": "x" });
        let err = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect_err("a misspelled key is not shimmed");
        assert_eq!(err.stderr, "unknown argument(s): temlate");
        assert_eq!(err.exit_code, 1);
    }

    /// Every shimmed row names a live tool that routes straight to the command
    /// of the same name, and whose schema really has dropped the key. Kills a
    /// renamed tool leaving a dead row, and a property re-added while the shim
    /// still exempts it (#597).
    #[test]
    fn deprecated_kwargs_name_live_tools_and_absent_properties() {
        let registry = register_all();
        let tools = build_tools(&registry);
        let routes = tool_routes(&registry);
        for (tool, keys) in DEPRECATED_KWARGS {
            assert_ne!(
                *tool, "load_template",
                "load_template refuses both keys outright; it must not be shimmed"
            );
            let route = routes
                .get(*tool)
                .unwrap_or_else(|| panic!("{tool} is not a synthesised tool"));
            // `dispatch_tool` looks the table up by `route.command`, so a tool
            // whose name differs from its command would never be shimmed.
            assert_eq!(route.command, *tool, "{tool} routes to {}", route.command);
            assert!(
                route.argv_prefix.is_empty(),
                "{tool}: {:?}",
                route.argv_prefix
            );
            let props = prop_names(&tools, tool);
            for key in *keys {
                assert!(
                    !props.contains(&(*key).to_owned()),
                    "{tool}.{key} is back in the schema; drop the shim row"
                );
            }
            assert!(!keys.is_empty(), "{tool}: an empty row shims nothing");
        }
    }

    /// The removal release, made enforceable: the version bump to 26.5 fails
    /// until the shim is gone (#597).
    #[test]
    fn deprecated_kwargs_expire_with_26_5() {
        let mut parts = env!("CARGO_PKG_VERSION").split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).expect("major");
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).expect("minor");
        assert!(
            (major, minor) < (26, 5) || DEPRECATED_KWARGS.is_empty(),
            "26.5 is here, so delete the one-release shim (#597). In \
             crates/mtui-mcp/src/tools.rs: the `DEPRECATED_KWARGS` table, the \
             `deprecated_kwargs` helper, and in `dispatch_tool` the `deprecated`/`sent` \
             block plus the `.chain(deprecated.iter().copied())` on `allowed`. Then \
             these tests: `deprecated_template_kwargs_are_ignored_on_session_level_tools`, \
             `regenerate_all_templates_kwarg_is_ignored`, \
             `typo_keys_are_still_refused_on_a_shimmed_tool`, \
             `deprecated_kwargs_name_live_tools_and_absent_properties` and this one. \
             Finally the CHANGELOG `### Deprecated` entry and the two-release sentence \
             in AGENTS.md's MCP contracts bullet."
        );
    }

    #[test]
    fn slow_commands_carry_background_others_do_not() {
        let tools = build_tools(&register_all());
        let run = descriptor(&tools, "run");
        let props = run
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(
            props.contains_key("background"),
            "run should carry background"
        );
        // `background` is optional (never required).
        if let Some(req) = run.input_schema.get("required") {
            let req: Vec<&str> = req
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(!req.contains(&"background"), "background must be optional");
        }

        let whoami = descriptor(&tools, "whoami");
        let props = whoami
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(
            !props.contains_key("background"),
            "non-slow whoami should not carry background"
        );
    }

    /// Both connect to whole fleets, so both must carry the `background` escape
    /// hatch: a black-hole candidate host must not wedge the caller.
    #[test]
    fn add_host_and_load_template_carry_background() {
        let tools = build_tools(&register_all());
        for name in ["add_host", "load_template"] {
            let props = descriptor(&tools, name)
                .input_schema
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap();
            assert!(
                props.contains_key("background"),
                "{name} should carry background"
            );
        }
    }

    #[test]
    fn read_only_hints_follow_allow_list() {
        let tools = build_tools(&register_all());
        for ro in ["whoami", "openqa_overview", "openqa_jobs", "list_hosts"] {
            assert!(descriptor(&tools, ro).read_only, "{ro} should be read-only");
        }
        for rw in ["run", "update", "approve", "reload_products"] {
            assert!(
                !descriptor(&tools, rw).read_only,
                "{rw} must not be read-only"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_config_show_routes_through_engine() {
        let mut config = Config::default();
        // `session_user` is refused on this surface (#410); drive a kept tunable.
        config.max_parallel = 7;
        let session = McpSession::new(config);
        let registry = register_all();
        let routes = tool_routes(&registry);
        let route = routes.get("config_show").expect("config_show route");
        assert_eq!(route.command, "config");
        assert_eq!(route.argv_prefix, vec!["show".to_owned()]);

        let registry = Arc::new(registry);
        let kwargs = json!({ "attributes": ["max_parallel"] });
        let out = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect("config show succeeds");
        assert!(out.contains("max_parallel"), "got: {out:?}");
        assert!(out.contains('7'), "got: {out:?}");
        // The filter has to survive argv reconstruction. Both assertions above
        // also hold of the unfiltered 39-attribute dump, so only the *absence*
        // of the other 38 proves `attributes` reached clap.
        assert!(!out.contains("connection_timeout"), "got: {out:?}");
        assert_eq!(
            out.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "only the requested attribute: {out:?}"
        );
    }

    /// The #410 surface holds through real dispatch: the bulk dump and an
    /// operator-local value are refused, while the REPL prints both.
    #[tokio::test]
    async fn dispatch_config_show_refuses_bulk_and_local_values() {
        let mut config = Config::default();
        config.session_user = "alice".to_owned();
        let session = McpSession::new(config);
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("config_show").expect("config_show route");

        // No `attributes`: the bulk dump is refused, not leaked by default.
        let err =
            completed(dispatch_tool(&registry, &session, route, &Map::new(), None, None).await)
                .expect_err("bulk dump refused");
        assert!(
            err.stderr.contains("name the attribute(s) explicitly"),
            "got: {err:?}"
        );

        // An explicitly named operator-local value is refused too.
        let kwargs = json!({ "attributes": ["session_user"] });
        let err = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await,
        )
        .expect_err("local value refused");
        assert!(
            err.stderr.contains("not exposed on this surface") && !err.stderr.contains("alice"),
            "got: {err:?}"
        );
    }

    /// The path a real client takes: `server.rs` calls `dispatch_tool`, which
    /// reconstructs argv from kwargs. `session.rs` pins the gate through
    /// `command_lock`/`run_command` on a hand-built argv, so it cannot see a
    /// reconstruction that drops both positionals (#523).
    #[tokio::test]
    async fn dispatch_config_set_mutates_the_canonical_session() {
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;

        let mut config = Config::default();
        config.session_user = "before".to_owned();
        let session = McpSession::new(config);
        // One loaded template: the state in which the gate's scoped arm forks.
        {
            let rrid = "SUSE:Maintenance:1:1";
            let mut guard = session.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
            guard.templates.add(Box::new(report));
            guard.templates.set_active(rrid);
        }

        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("config_set").expect("config_set route");
        let kwargs = json!({ "attribute": "session_user", "value": "via-tool" });
        let out = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect("config set succeeds");
        assert_eq!(out.trim(), "option: session_user set to value : via-tool");
        assert_eq!(
            session.session().lock().await.config.session_user,
            "via-tool",
            "the write must survive the call"
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_unknown_property_instead_of_dropping_it() {
        // Silently discarding it would run `config show` with no filter.
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("config_show").expect("config_show route");
        let kwargs = json!({ "attribut": ["session_user"] }); // typo: attribut(e)s
        let err = completed(
            dispatch_tool(
                &registry,
                &session,
                route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect_err("typo refused");
        assert_eq!(err.exit_code, 1);
        assert!(
            err.stderr.contains("unknown argument(s): attribut"),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_still_accepts_background_on_slow_route() {
        // Popped before validation, so a legit background call is not rejected.
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("run").expect("run route").clone();
        assert!(route.slow, "run must be slow");
        let kwargs = json!({ "background": true, "command": ["true"] });
        let out = completed(
            dispatch_tool(
                &registry,
                &session,
                &route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect("background start not rejected");
        assert!(out.contains("started job"), "got: {out:?}");
    }

    /// A `background=true` slow call with nothing loaded mints one job and
    /// returns the single-job "started job" reply naming the id to poll.
    #[tokio::test]
    async fn dispatch_background_true_starts_a_job() {
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let routes = tool_routes(&registry);
        let route = routes.get("run").expect("run route").clone();
        assert!(route.slow, "run must be slow");

        // `run` needs a command to execute; supply one so argv reconstructs.
        let kwargs = json!({ "background": true, "command": ["true"] });
        let reply = completed(
            dispatch_tool(
                &registry,
                &session,
                &route,
                kwargs.as_object().unwrap(),
                None,
                None,
            )
            .await
            .outcome,
        )
        .expect("background start returns a reply, not an error");
        assert_eq!(reply, SINGLE_JOB_REPLY);
    }

    /// The reply a model reads the instant a job starts — the highest-leverage
    /// place to name the wait, so both forms lead with the blocking call rather
    /// than trailing it after "poll". Pinned whole: a substring check here would
    /// survive exactly the regression #624 is about.
    #[test]
    fn started_jobs_reply_pins_both_forms() {
        assert_eq!(
            started_jobs_reply("run", &["run-1".to_owned()]),
            SINGLE_JOB_REPLY
        );
        assert_eq!(
            started_jobs_reply("run", &["run-1".to_owned(), "run-2".to_owned()]),
            "started 2 jobs (`run`, one per template): 'run-1', 'run-2'. job_result(id, wait_seconds=N) blocks up to N s per job; job_status(id, wait_seconds=N) for state."
        );
    }

    /// The single-job form, shared by the dispatch-level and unit-level pins.
    const SINGLE_JOB_REPLY: &str = "started job 'run-1' (`run`); job_result('run-1', wait_seconds=N) blocks up to N s for its output; job_status('run-1', wait_seconds=N) for state only.";

    #[test]
    fn job_tools_have_correct_read_only_hints() {
        let tools = job_tool_descriptors();
        assert_eq!(
            names(&tools),
            ["job_list", "job_status", "job_result", "job_cancel"]
        );
        for ro in ["job_list", "job_status", "job_result"] {
            assert!(descriptor(&tools, ro).read_only, "{ro} read-only");
        }
        assert!(
            !descriptor(&tools, "job_cancel").read_only,
            "job_cancel not read-only"
        );
    }

    /// `job_list` on a fresh session reports no jobs.
    #[tokio::test]
    async fn dispatch_job_list_empty() {
        let session = McpSession::new(Config::default());
        let out = dispatch_job_tool(&session, "job_list", &Map::new(), None)
            .await
            .expect("job_list succeeds");
        assert_eq!(out, "no background jobs");
    }

    /// A job tool refuses a misspelled property rather than ignoring it.
    #[tokio::test]
    async fn dispatch_job_tool_refuses_unknown_property() {
        let session = McpSession::new(Config::default());
        // `job_list` takes no args.
        let kwargs = json!({ "job_id": "x" });
        let err = dispatch_job_tool(&session, "job_list", kwargs.as_object().unwrap(), None)
            .await
            .expect_err("job_list takes nothing");
        assert!(
            err.stderr.contains("unknown argument(s): job_id"),
            "got: {err:?}"
        );
        // `job_status` takes only `job_id`.
        let kwargs = json!({ "job_id": "x", "jub_id": "typo" });
        let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
            .await
            .expect_err("typo refused");
        assert!(
            err.stderr.contains("unknown argument(s): jub_id"),
            "got: {err:?}"
        );
    }

    /// `job_status` requires a `job_id` (parse-style error when absent).
    #[tokio::test]
    async fn dispatch_job_status_requires_job_id() {
        let session = McpSession::new(Config::default());
        let err = dispatch_job_tool(&session, "job_status", &Map::new(), None)
            .await
            .expect_err("missing job_id fails");
        assert_eq!(err.exit_code, 2, "missing arg is a parse error");
        assert!(err.stderr.contains("job_id"), "names the arg: {err:?}");
    }

    /// `job_status` on an unknown id surfaces the "no such job" envelope.
    #[tokio::test]
    async fn dispatch_job_status_unknown_id() {
        let session = McpSession::new(Config::default());
        let kwargs = json!({ "job_id": "nope-1" });
        let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
            .await
            .expect_err("unknown id fails");
        assert!(err.stderr.contains("no such job: nope-1"), "got: {err:?}");
    }

    /// The pinned text shapes: `- id: state (…s) [cmd]` for `job_list`, without
    /// the dash for `job_status`.
    #[tokio::test]
    async fn dispatch_job_list_and_status_render_started_job() {
        let mut config = Config::default();
        config.session_user = "bob".to_owned();
        let session = McpSession::new(config);
        let registry = Arc::new(register_all());

        let job_id = session
            .start_job(Arc::clone(&registry), "whoami", Vec::new())
            .expect("start_job succeeds");

        let listed = dispatch_job_tool(&session, "job_list", &Map::new(), None)
            .await
            .expect("job_list succeeds");
        assert!(
            listed.starts_with(&format!("- {job_id}: ")),
            "job_list line prefixed with '- ': {listed:?}"
        );
        assert!(listed.contains("[whoami]"), "names the command: {listed:?}");

        let kwargs = json!({ "job_id": job_id });
        let status = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
            .await
            .expect("job_status succeeds");
        assert!(
            !status.starts_with("- "),
            "job_status has no '- ' prefix: {status:?}"
        );
        assert!(status.contains("[whoami]"), "names the command: {status:?}");
    }

    // ---- wait_seconds (#624) ----------------------------------------------- //

    /// A recording [`ProgressSink`] double (mirrors `session.rs`'s).
    #[derive(Default)]
    struct RecordingSink {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl ProgressSink for RecordingSink {
        fn report<'a>(
            &'a self,
            _progress: f64,
            message: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            let message = message.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push(message);
            })
        }
    }

    /// A test-only command whose body blocks until its gate is released, so a
    /// job built on it stays `running` for as long as the test needs.
    struct GatedProbe(Arc<tokio::sync::Notify>);

    #[async_trait]
    impl Command for GatedProbe {
        fn name(&self) -> &'static str {
            "gated_wait_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Fanout
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            self.0.notified().await;
            Ok(())
        }
    }

    /// Start a `gated_wait_probe` job that never finishes on its own.
    fn blocked_job(session: &Arc<McpSession>) -> String {
        let mut registry = register_all();
        registry.register(Arc::new(GatedProbe(Arc::new(tokio::sync::Notify::new()))));
        session
            .start_job(Arc::new(registry), "gated_wait_probe", Vec::new())
            .expect("start_job succeeds")
    }

    /// A test-only command whose body takes a fixed, short time to finish and
    /// prints, so a waiting `job_result` has something to hand back.
    struct SlowProbe;

    #[async_trait]
    impl Command for SlowProbe {
        fn name(&self) -> &'static str {
            "slow_wait_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Fanout
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            tokio::time::sleep(Duration::from_millis(200)).await;
            session.display.println("slow probe finished");
            Ok(())
        }
    }

    /// `wait_seconds` is an accepted argument on `job_status` — an explicit
    /// `null` included, which the schema's `default: 0` documents as the plain
    /// poll — and the reply is still the job's own status line. *Which* state it
    /// reports is timing-dependent and pinned in `tests/mcp_jobs.rs`, not here.
    #[tokio::test]
    async fn dispatch_job_status_wait_seconds_is_accepted() {
        let session = McpSession::new(Config::default());
        let registry = Arc::new(register_all());
        let job_id = session
            .start_job(registry, "whoami", Vec::new())
            .expect("start_job succeeds");

        for budget in [json!(5), Value::Null] {
            let kwargs = json!({ "job_id": job_id, "wait_seconds": budget });
            let out = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
                .await
                .expect("wait_seconds is a known argument");
            assert!(out.starts_with(&format!("{job_id}: ")), "got: {out:?}");
        }
    }

    /// `job_result` takes the same budget and hands back the job's own output
    /// once it settles inside it.
    #[tokio::test]
    async fn dispatch_job_result_wait_seconds_is_accepted() {
        let session = McpSession::new(Config::default());
        let mut registry = register_all();
        registry.register(Arc::new(SlowProbe));
        let job_id = session
            .start_job(Arc::new(registry), "slow_wait_probe", Vec::new())
            .expect("start_job succeeds");

        let kwargs = json!({ "job_id": job_id, "wait_seconds": 5 });
        let out = dispatch_job_tool(&session, "job_result", kwargs.as_object().unwrap(), None)
            .await
            .expect("the wait outlasts the job, so the output is ready");
        assert_eq!(out.trim(), "slow probe finished");
    }

    /// A zero budget on `job_result` is the plain fetch the server routes
    /// unwrapped: no park, and the unchanged still-running envelope at exit 1.
    #[tokio::test]
    async fn dispatch_job_result_wait_zero_keeps_the_still_running_error() {
        let session = McpSession::new(Config::default());
        let job_id = blocked_job(&session);

        let kwargs = json!({ "job_id": job_id, "wait_seconds": 0 });
        let err = tokio::time::timeout(
            Duration::from_secs(1),
            dispatch_job_tool(&session, "job_result", kwargs.as_object().unwrap(), None),
        )
        .await
        .expect("a zero budget returns without parking")
        .expect_err("the job is still running");

        assert_eq!(err.exit_code, 1, "got: {err:?}");
        let (head, tail) = err
            .stderr
            .split_once(" (")
            .unwrap_or_else(|| panic!("no elapsed parenthetical: {err:?}"));
        assert_eq!(head, format!("job {job_id} still running"));
        let (elapsed, rest) = tail
            .split_once("s); ")
            .unwrap_or_else(|| panic!("no elapsed seconds: {err:?}"));
        assert!(
            elapsed.parse::<f64>().is_ok(),
            "the elapsed is a float: {elapsed:?}"
        );
        assert_eq!(rest, "job_result(wait_seconds=N) waits for it");
    }

    /// A zero budget must not park: the server routes it unwrapped, so a dispatch
    /// that floored it to a second (`secs.max(1)`) would split the two paths the
    /// design keeps identical.
    #[tokio::test]
    async fn dispatch_job_status_wait_zero_does_not_park() {
        let session = McpSession::new(Config::default());
        let job_id = blocked_job(&session);

        let kwargs = json!({ "job_id": job_id, "wait_seconds": 0 });
        let out = tokio::time::timeout(
            Duration::from_millis(300),
            dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None),
        )
        .await
        .expect("a zero budget returns without parking")
        .expect("job exists");
        assert!(out.contains(" running "), "got: {out:?}");
    }

    /// ...and a one-second budget is one second: neither scaled (`secs * 10`) nor
    /// silently promoted to the cap, and it really does park rather than falling
    /// straight through.
    #[tokio::test]
    async fn dispatch_job_status_wait_one_second_parks_for_one_second() {
        let session = McpSession::new(Config::default());
        let job_id = blocked_job(&session);

        let kwargs = json!({ "job_id": job_id, "wait_seconds": 1 });
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_millis(1500),
            dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None),
        )
        .await
        .expect("the budget bounds the wait")
        .expect("job exists");
        let elapsed = started.elapsed();
        assert!(out.contains(" running "), "got: {out:?}");
        assert!(
            elapsed >= Duration::from_millis(900),
            "the budget was actually parked on: {elapsed:?}"
        );
    }

    /// Over the cap is refused, not clamped, and the message names both bounds —
    /// including a JSON integer past `i64::MAX`, which is an over-cap budget and
    /// not a malformed one.
    #[tokio::test]
    async fn dispatch_job_status_refuses_wait_seconds_above_cap() {
        let session = McpSession::new(Config::default());
        for over in [json!(JOB_WAIT_CAP_SECS + 1), json!(u64::MAX)] {
            let kwargs = json!({ "job_id": "x-1", "wait_seconds": over });
            let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
                .await
                .expect_err("over-cap is a client bug");
            assert_eq!(err.exit_code, 2, "parse-style refusal: {err:?}");
            assert_eq!(
                err.stderr,
                format!("wait_seconds must be <= {JOB_WAIT_CAP_SECS} (got {over})")
            );
        }
    }

    /// A negative budget is refused rather than silently floored to zero.
    #[tokio::test]
    async fn dispatch_job_status_refuses_negative_wait_seconds() {
        let session = McpSession::new(Config::default());
        let kwargs = json!({ "job_id": "x-1", "wait_seconds": -1 });
        let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
            .await
            .expect_err("negative is refused");
        assert_eq!(err.exit_code, 2, "got: {err:?}");
        assert_eq!(err.stderr, "wait_seconds must be >= 0 (got -1)");
    }

    /// A string or a fraction is refused; neither is an integer second count.
    #[tokio::test]
    async fn dispatch_job_status_refuses_non_integer_wait_seconds() {
        let session = McpSession::new(Config::default());
        for bad in [json!("5"), json!(1.5)] {
            let kwargs = json!({ "job_id": "x-1", "wait_seconds": bad });
            let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap(), None)
                .await
                .expect_err("non-integer is refused");
            assert_eq!(err.exit_code, 2, "got: {err:?}");
            assert!(
                err.stderr.starts_with("wait_seconds must be an integer"),
                "got: {err:?}"
            );
        }
    }

    /// `job_cancel` advertises no `wait_seconds`, so passing one is a clean
    /// unknown-argument refusal rather than a silently ignored field.
    #[tokio::test]
    async fn dispatch_job_cancel_refuses_wait_seconds() {
        let session = McpSession::new(Config::default());
        let kwargs = json!({ "job_id": "x-1", "wait_seconds": 5 });
        let err = dispatch_job_tool(&session, "job_cancel", kwargs.as_object().unwrap(), None)
            .await
            .expect_err("job_cancel takes no budget");
        assert!(
            err.stderr.contains("unknown argument(s): wait_seconds"),
            "got: {err:?}"
        );
    }

    /// A parked wait feeds the heartbeat sink, so a client that reset its read
    /// deadline on progress does not time the held request out.
    #[tokio::test]
    async fn dispatch_job_status_wait_emits_heartbeats() {
        let session = McpSession::new(Config::default());
        let mut registry = register_all();
        registry.register(Arc::new(SlowProbe));
        let job_id = session
            .start_job(Arc::new(registry), "slow_wait_probe", Vec::new())
            .expect("start_job succeeds");

        let sink = RecordingSink::default();
        let kwargs = json!({ "job_id": job_id, "wait_seconds": 1 });
        dispatch_job_tool_with_interval(
            &session,
            "job_status",
            kwargs.as_object().unwrap(),
            Some(&sink),
            Duration::from_millis(40),
        )
        .await
        .expect("the wait succeeds");
        let frames = sink.calls.lock().unwrap().clone();
        assert!(!frames.is_empty(), "a parked wait must heartbeat");
        assert!(
            frames.iter().all(|f| f.contains("job_status")),
            "frames name the tool: {frames:?}"
        );
    }

    /// The advertised bounds and the validator are one contract: the schema's
    /// `maximum` is the constant the dispatch refuses past, and a tool without
    /// the property refuses the argument outright.
    #[tokio::test]
    async fn job_wait_schema_bounds_match_the_validator() {
        let tools = job_tool_descriptors();
        let wait_prop = |name: &str| {
            descriptor(&tools, name)
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|p| p.get("wait_seconds"))
                .cloned()
        };

        for tool in ["job_status", "job_result"] {
            let prop = wait_prop(tool).unwrap_or_else(|| panic!("{tool} advertises wait_seconds"));
            assert_eq!(prop["maximum"], json!(JOB_WAIT_CAP_SECS), "{tool}");
            assert_eq!(prop["minimum"], json!(0), "{tool}");
            assert_eq!(prop["default"], json!(0), "{tool}");
        }
        assert!(
            wait_prop("job_cancel").is_none(),
            "job_cancel must not advertise a budget"
        );

        // The advertised maximum is exactly the last value the dispatch accepts.
        let session = McpSession::new(Config::default());
        let at_cap = json!({ "job_id": "x-1", "wait_seconds": JOB_WAIT_CAP_SECS });
        let err = dispatch_job_tool(&session, "job_status", at_cap.as_object().unwrap(), None)
            .await
            .expect_err("no such job");
        assert!(
            err.stderr.contains("no such job"),
            "the cap itself parses: {err:?}"
        );
    }

    /// The server's wrap decision: only a positive integer budget on a waiting
    /// tool parks, so nothing else pays for the cancellation/heartbeat wrapper.
    #[test]
    fn job_call_waits_predicate() {
        let cases = [
            (
                "job_status",
                json!({ "job_id": "a", "wait_seconds": 5 }),
                true,
            ),
            (
                "job_status",
                json!({ "job_id": "a", "wait_seconds": 0 }),
                false,
            ),
            ("job_status", json!({ "job_id": "a" }), false),
            (
                "job_status",
                json!({ "job_id": "a", "wait_seconds": Value::Null }),
                false,
            ),
            (
                "job_status",
                json!({ "job_id": "a", "wait_seconds": "5" }),
                false,
            ),
            // Past `i64::MAX`: the server routes it unwrapped and the dispatch
            // refuses it against the cap — never a 2^64-second park.
            (
                "job_status",
                json!({ "job_id": "a", "wait_seconds": u64::MAX }),
                false,
            ),
            (
                "job_result",
                json!({ "job_id": "a", "wait_seconds": 5 }),
                true,
            ),
            ("job_result", json!({ "job_id": "a" }), false),
            (
                "job_cancel",
                json!({ "job_id": "a", "wait_seconds": 5 }),
                false,
            ),
            ("job_list", json!({}), false),
        ];
        for (name, kwargs, want) in cases {
            assert_eq!(
                job_call_waits(name, kwargs.as_object().unwrap()),
                want,
                "{name} with {kwargs}"
            );
        }
    }

    /// An unrecognised job-tool name is a clean error (defensive: the server
    /// only routes the four known names here).
    #[tokio::test]
    async fn dispatch_job_tool_unknown_name() {
        let session = McpSession::new(Config::default());
        let err = dispatch_job_tool(&session, "job_bogus", &Map::new(), None)
            .await
            .expect_err("unknown job tool fails");
        assert!(err.stderr.contains("unknown job tool"), "got: {err:?}");
    }
}
