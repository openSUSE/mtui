//! Synthesise MCP tools from the command [`Registry`].
//!
//! Port of upstream `mtui/mcp/tools.py`. For every command in the registry that
//! is not on the [`crate::deny`] list, this module builds one plain-data
//! [`ToolDescriptor`] whose:
//!
//! * **name** is the command name (e.g. `run`);
//! * **description** is the command's [`about`](mtui_core::Command::about);
//! * **`input_schema`** is derived from the command's built `clap` parser via
//!   [`crate::schema::command_input_schema`];
//! * **`read_only`** hint is set conservatively from a name allow-list.
//!
//! The subparser command (`config` today) is fanned out into one tool per
//! subcommand (`config_show`, `config_set`); the bare `config` tool is not
//! emitted (a "show or set" union schema would mislead the client about which
//! fields are required). Slow host commands gain a `background` boolean.
//!
//! This layer is intentionally **transport-free**: it returns plain descriptors
//! and routes, not `rmcp` types. P7.7 converts a [`ToolDescriptor`] into an
//! `rmcp::model::Tool` and wires [`dispatch_tool`] into the `ServerHandler`.
//!
//! The background-job path ([`dispatch_tool`] with `background = true`, and the
//! four job tools from [`job_tool_descriptors`]) drives the session's `_jobs`
//! table (bead `mtui-rs-76e.12`): a `background=true` slow call fans out one job
//! per resolved template and returns their ids immediately, and the four job
//! tools poll/control that table.

use std::collections::BTreeMap;
use std::sync::Arc;

use mtui_core::{Registry, command_parser};
use serde_json::{Map, Value, json};

use crate::deny::is_denied;
use crate::schema::command_input_schema;
use crate::session::{
    DEFAULT_PROGRESS_INTERVAL, JobView, McpCommandError, McpSession, ProgressSink,
};

/// Commands that touch reference hosts and can run for minutes. These gain a
/// `background` boolean parameter (see [`dispatch_tool`]). Pinned here, matching
/// upstream `tools.SLOW_COMMANDS`.
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
];

/// The one command whose `clap` subcommands are fanned out into per-subcommand
/// tools. Pinned (not auto-discovered) so the surface is stable and visible.
const SUBPARSER_COMMANDS: &[&str] = &["config"];

/// A command becomes `read_only` if its name starts with one of these prefixes.
const READ_ONLY_PREFIXES: &[&str] = &["list_", "show_"];

/// Exact names that escape the prefix rule but are still side-effect-free.
/// (`reload_products` is intentionally absent — it re-reads from the hosts.)
const READ_ONLY_EXACT: &[&str] = &["whoami", "openqa_overview", "openqa_jobs"];

/// A synthesised MCP tool as plain data (transport-free).
///
/// P7.7 converts this into `rmcp::model::Tool` (name + description +
/// `Arc<input_schema>` + `ToolAnnotations { read_only_hint }`).
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

/// How a tool name routes back to the engine when called.
///
/// Built in the same pass as the descriptors so a tool's schema and its dispatch
/// can never diverge. [`dispatch_tool`] consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRoute {
    /// The registry command name to dispatch (`config` for `config_show`).
    pub command: &'static str,
    /// Tokens prepended to the reconstructed argv (`["show"]` for `config_show`).
    pub argv_prefix: Vec<String>,
    /// Whether this tool accepts the `background` flag (a slow host command).
    pub slow: bool,
}

/// `true` iff a command is known to be side-effect-free.
fn is_read_only(name: &str) -> bool {
    READ_ONLY_EXACT.contains(&name) || READ_ONLY_PREFIXES.iter().any(|p| name.starts_with(p))
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
                    poll job_status/job_result.",
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
        if is_denied(name) {
            continue;
        }
        let command = registry
            .get(name)
            .expect("registry.names() yields registered commands");

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

/// Register one tool per subcommand of a subparser command (`config`).
///
/// The bare parent tool is not emitted. `config` is not slow, so no `background`.
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

/// Build the synthesised command-tool descriptors, sorted by name.
///
/// Skips deny-listed commands, fans out the `config` subparser, and injects a
/// `background` flag into slow host commands. Does not include the job tools —
/// see [`job_tool_descriptors`].
#[must_use]
pub fn build_tools(registry: &Registry) -> Vec<ToolDescriptor> {
    synthesise(registry).0
}

/// Build the tool-name → [`ToolRoute`] map for dispatching calls back to the
/// engine. Keys match [`build_tools`] descriptor names exactly.
#[must_use]
pub fn tool_routes(registry: &Registry) -> BTreeMap<String, ToolRoute> {
    synthesise(registry).1
}

/// Dispatch a synthesised command tool call back through the engine.
///
/// Pops the `background` flag for slow commands; when `true` the call fans out
/// background jobs via [`McpSession::start_jobs`] (one per resolved template) and
/// returns a "started job(s)" reply naming the ids to poll. Otherwise
/// reconstructs argv from `kwargs` (honouring the route's `argv_prefix`) and runs
/// it synchronously through [`McpSession::run_command_with_progress`], emitting
/// heartbeats via `sink` (when the client requested progress) so a slow
/// foreground call does not time the client out.
///
/// # Errors
///
/// Returns [`McpCommandError`] when the command is not registered, or when the
/// synchronous parse/run fails (propagated from
/// [`McpSession::run_command_with_progress`]).
pub async fn dispatch_tool(
    registry: &Arc<Registry>,
    session: &Arc<McpSession>,
    route: &ToolRoute,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
) -> Result<String, McpCommandError> {
    let mut kwargs = kwargs.clone();
    let background = if route.slow {
        matches!(kwargs.remove("background"), Some(Value::Bool(true)))
    } else {
        false
    };

    let command = registry.get(route.command).ok_or_else(|| McpCommandError {
        stdout: String::new(),
        stderr: format!("command not registered: {}", route.command),
        exit_code: 1,
    })?;
    let parser = command_parser(command.as_ref());
    let argv = crate::argv::kwargs_to_argv(&parser, &kwargs, &route.argv_prefix);

    if background {
        let job_ids = session
            .start_jobs(Arc::clone(registry), route.command, argv)
            .await;
        return Ok(started_jobs_reply(route.command, &job_ids));
    }

    session
        .run_command_with_progress(
            registry,
            route.command,
            &argv,
            sink,
            DEFAULT_PROGRESS_INTERVAL,
        )
        .await
}

/// The client-facing reply after starting one or more background jobs.
///
/// Matches upstream `tools.py` verbatim: a single job points at
/// `job_status`/`job_result` for that id; a fan-out lists every id and tells the
/// client to poll per job.
fn started_jobs_reply(command: &str, job_ids: &[String]) -> String {
    if let [job_id] = job_ids {
        return format!(
            "started job '{job_id}' (`{command}`); poll job_status('{job_id}'), \
             then job_result('{job_id}')."
        );
    }
    let joined = job_ids
        .iter()
        .map(|j| format!("'{j}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "started {} jobs (`{command}`, one per template): {joined}. \
         Poll job_status/job_result per job.",
        job_ids.len()
    )
}

/// The four background-job control tools (stubbed until `mtui-rs-76e.12`).
///
/// Their schemas and descriptions are final; only the handlers
/// ([`dispatch_job_tool`]) are stubbed.
#[must_use]
pub fn job_tool_descriptors() -> Vec<ToolDescriptor> {
    let job_id_schema = || {
        let mut props = Map::new();
        props.insert(
            "job_id".to_owned(),
            json!({ "type": "string", "description": "The background job id." }),
        );
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(props));
        schema.insert("required".to_owned(), json!(["job_id"]));
        schema
    };
    let empty_schema = || {
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(Map::new()));
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
            description: "Report a background job's state and elapsed time. Poll this \
                after starting a slow command with background=true."
                .to_owned(),
            input_schema: job_id_schema(),
            read_only: true,
        },
        ToolDescriptor {
            name: "job_result".to_owned(),
            description: "Return a finished background job's output. Errors if the job \
                is still running or surfaces the command's failure if it failed."
                .to_owned(),
            input_schema: job_id_schema(),
            read_only: true,
        },
        ToolDescriptor {
            name: "job_cancel".to_owned(),
            description: "Cancel a running background job. A job already executing on a \
                host may keep running there even after cancel."
                .to_owned(),
            input_schema: job_id_schema(),
            read_only: false,
        },
    ]
}

/// Dispatch a job-control tool call against the session's `_jobs` table.
///
/// Routes `job_list` / `job_status` / `job_result` / `job_cancel` to the
/// matching [`McpSession`] method and renders its result into the one-line text
/// the client sees (upstream `tools.py` job tools).
///
/// # Errors
///
/// Returns [`McpCommandError`] when a `job_id` is missing/unknown, when
/// `job_result` is polled on a still-running / failed / cancelled job, or when
/// the tool name is unrecognised.
pub async fn dispatch_job_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
) -> Result<String, McpCommandError> {
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
            Ok(format_job_view(&session.job_status(&job_id)?))
        }
        "job_result" => {
            let job_id = job_id_arg(kwargs)?;
            session.job_result(&job_id)
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

/// Render a [`JobView`] as the one-line status text (`job_status` form).
///
/// `job_list` prepends `"- "` to each of these.
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

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::Config;
    use mtui_core::register_all;

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
        for denied in [
            "quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch",
        ] {
            assert!(
                !names(&tools).contains(&denied),
                "denied command {denied} leaked into tools"
            );
        }
    }

    #[test]
    fn config_is_fanned_out_bare_config_absent() {
        let tools = build_tools(&register_all());
        let ns = names(&tools);
        assert!(!ns.contains(&"config"), "bare config must not be a tool");
        assert!(ns.contains(&"config_show"), "config_show missing");
        assert!(ns.contains(&"config_set"), "config_set missing");
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
        config.session_user = "alice".to_owned();
        let session = McpSession::new(config);
        let registry = register_all();
        let routes = tool_routes(&registry);
        let route = routes.get("config_show").expect("config_show route");
        assert_eq!(route.command, "config");
        assert_eq!(route.argv_prefix, vec!["show".to_owned()]);

        let registry = Arc::new(registry);
        let kwargs = json!({ "attributes": ["session_user"] });
        let out = dispatch_tool(
            &registry,
            &session,
            route,
            kwargs.as_object().unwrap(),
            None,
        )
        .await
        .expect("config show succeeds");
        assert!(out.contains("session_user"), "got: {out:?}");
        assert!(out.contains("alice"), "got: {out:?}");
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
        let reply = dispatch_tool(
            &registry,
            &session,
            &route,
            kwargs.as_object().unwrap(),
            None,
        )
        .await
        .expect("background start returns a reply, not an error");
        assert!(
            reply.starts_with("started job 'run-1' (`run`);"),
            "single-job reply names the id: {reply:?}"
        );
        assert!(
            reply.contains("job_status('run-1')") && reply.contains("job_result('run-1')"),
            "reply points at the poll tools: {reply:?}"
        );
    }

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
        let out = dispatch_job_tool(&session, "job_list", &Map::new())
            .await
            .expect("job_list succeeds");
        assert_eq!(out, "no background jobs");
    }

    /// `job_status` requires a `job_id` (parse-style error when absent).
    #[tokio::test]
    async fn dispatch_job_status_requires_job_id() {
        let session = McpSession::new(Config::default());
        let err = dispatch_job_tool(&session, "job_status", &Map::new())
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
        let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap())
            .await
            .expect_err("unknown id fails");
        assert!(err.stderr.contains("no such job: nope-1"), "got: {err:?}");
    }

    /// A started job renders through `job_list` / `job_status` in the pinned
    /// text shapes (`- id: state (…s) [cmd]` vs `id: state (…s) [cmd]`).
    #[tokio::test]
    async fn dispatch_job_list_and_status_render_started_job() {
        let mut config = Config::default();
        config.session_user = "bob".to_owned();
        let session = McpSession::new(config);
        let registry = Arc::new(register_all());

        let job_id = session.start_job(Arc::clone(&registry), "whoami", Vec::new());

        let listed = dispatch_job_tool(&session, "job_list", &Map::new())
            .await
            .expect("job_list succeeds");
        assert!(
            listed.starts_with(&format!("- {job_id}: ")),
            "job_list line prefixed with '- ': {listed:?}"
        );
        assert!(listed.contains("[whoami]"), "names the command: {listed:?}");

        let kwargs = json!({ "job_id": job_id });
        let status = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap())
            .await
            .expect("job_status succeeds");
        assert!(
            !status.starts_with("- "),
            "job_status has no '- ' prefix: {status:?}"
        );
        assert!(status.contains("[whoami]"), "names the command: {status:?}");
    }

    /// An unrecognised job-tool name is a clean error (defensive; the server
    /// only routes the four known names here).
    #[tokio::test]
    async fn dispatch_job_tool_unknown_name() {
        let session = McpSession::new(Config::default());
        let err = dispatch_job_tool(&session, "job_bogus", &Map::new())
            .await
            .expect_err("unknown job tool fails");
        assert!(err.stderr.contains("unknown job tool"), "got: {err:?}");
    }
}
