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
use std::sync::Arc;

use mtui_core::{Registry, command_parser};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::deny::is_denied;
use crate::schema::command_input_schema;
use crate::session::{
    DEFAULT_PROGRESS_INTERVAL, JobView, McpCommandError, McpSession, ProgressSink, ToolOutcome,
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
) -> ToolOutcome {
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
    let allowed = arg_source
        .get_arguments()
        .map(|a| a.get_id().as_str())
        .filter(|id| *id != "help" && *id != "version");
    if let Err(err) = reject_unknown_kwargs(&kwargs, allowed) {
        return Err::<String, _>(err).into();
    }

    // The same layer, or reconstruction drops every kwarg the parent does not
    // declare: `config set` emitted a bare `["set"]` and clap rejected it for the
    // missing required positionals, `config show`'s filter vanished. The parent's
    // own `-T`/`--all-templates` are not lost, because a fanned-out tool's schema
    // is synthesised from the subcommand too, so they are already refused above.
    let argv = crate::argv::kwargs_to_argv(arg_source, &kwargs, &route.argv_prefix);

    if background {
        return session
            .start_jobs(Arc::clone(registry), route.command, argv)
            .await
            .map(|job_ids| started_jobs_reply(route.command, &job_ids))
            .into();
    }

    match client_ct {
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
    }
}

/// The client-facing reply after starting one or more background jobs: a single
/// job points at `job_status`/`job_result`, a fan-out lists every id.
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

/// The four background-job control tools, routed onto the session's job table by
/// `dispatch_job_tool`.
///
/// Their names and schemas are a downstream contract and snapshot-tested; the
/// schemas are strict (`additionalProperties: false`), so a misspelled field is a
/// clean error rather than a silently ignored argument.
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
                host may keep running there even after cancel; the operation lock the \
                job's own host group took is released best-effort (bounded, and never \
                a comment-marked reservation) and the reply reports the outcome."
                .to_owned(),
            input_schema: job_id_schema(),
            read_only: false,
        },
    ]
}

/// Dispatch a job-control tool call against the session's `_jobs` table.
///
/// Routes each of the four names to the matching [`McpSession`] method and
/// renders its result into the one-line text the client sees.
///
/// # Errors
///
/// Returns [`McpCommandError`] when a `job_id` is missing/unknown, when
/// `job_result` is polled on a still-running / failed / cancelled job, or when
/// the tool name is unrecognised.
pub(crate) async fn dispatch_job_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
) -> Result<String, McpCommandError> {
    // Mirrors each job tool's strict schema.
    let allowed: &[&str] = if name == "job_list" { &[] } else { &["job_id"] };
    reject_unknown_kwargs(kwargs, allowed.iter().copied())?;

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
        for denied in [
            "quit", "exit", "EOF", "edit", "shell", "help", "terms", "switch",
        ] {
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
        config.session_user = "alice".to_owned();
        let session = McpSession::new(config);
        let registry = register_all();
        let routes = tool_routes(&registry);
        let route = routes.get("config_show").expect("config_show route");
        assert_eq!(route.command, "config");
        assert_eq!(route.argv_prefix, vec!["show".to_owned()]);

        let registry = Arc::new(registry);
        let kwargs = json!({ "attributes": ["session_user"] });
        let out = completed(
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
        .expect("config show succeeds");
        assert!(out.contains("session_user"), "got: {out:?}");
        assert!(out.contains("alice"), "got: {out:?}");
        // The filter has to survive argv reconstruction. Both assertions above
        // also hold of the unfiltered 41-attribute dump, so only the *absence*
        // of the other 40 proves `attributes` reached clap.
        assert!(!out.contains("template_dir"), "got: {out:?}");
        assert_eq!(
            out.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "only the requested attribute: {out:?}"
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
            .await,
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
            .await,
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
            .await,
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
            .await,
        )
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

    /// A job tool refuses a misspelled property rather than ignoring it.
    #[tokio::test]
    async fn dispatch_job_tool_refuses_unknown_property() {
        let session = McpSession::new(Config::default());
        // `job_list` takes no args.
        let kwargs = json!({ "job_id": "x" });
        let err = dispatch_job_tool(&session, "job_list", kwargs.as_object().unwrap())
            .await
            .expect_err("job_list takes nothing");
        assert!(
            err.stderr.contains("unknown argument(s): job_id"),
            "got: {err:?}"
        );
        // `job_status` takes only `job_id`.
        let kwargs = json!({ "job_id": "x", "jub_id": "typo" });
        let err = dispatch_job_tool(&session, "job_status", kwargs.as_object().unwrap())
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

    /// An unrecognised job-tool name is a clean error (defensive: the server
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
