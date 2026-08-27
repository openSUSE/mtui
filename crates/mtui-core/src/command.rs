//! The `Command` trait, its fan-out [`Scope`], and the template fan-out engine.
//!
//! Every command implements [`Command`] and is discovered through the registry —
//! the one thing the REPL, tab completion, and the MCP tool synthesiser all
//! iterate. A command supplies its abstract body in [`call`](Command::call); the
//! provided [`run`](Command::run) drives it across the templates the invocation
//! resolves to:
//!
//! * `-T/--template RRID` — exactly that loaded template.
//! * `--all-templates` or [`Scope::Fanout`] — every loaded template.
//! * [`Scope::Single`] — exactly once, never auto-fanned-out (self-targeting
//!   commands like `unload <rrid>`).
//! * otherwise the active template — except headlessly (MCP, `interactive =
//!   false`) with more than one loaded, where there is no addressable active
//!   pointer, so the call fans out.
//!
//! Fan-out gives each template a banner and its own error boundary, skips a
//! host-less template when no `-t` host was named, and raises a
//! [`CommandError::FanOut`] if any failed. Every template skipped means the
//! command ran nowhere: [`CommandError::NoRefhostsDefined`], not a silent
//! success.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Fan-out scope policy for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Run once against the active template. Under MCP with several templates
    /// loaded this defaults to fan-out (there is no addressable active pointer).
    #[default]
    Active,
    /// Run once per loaded template. Action commands safe to repeat opt in.
    Fanout,
    /// Run exactly once regardless of how many templates are loaded — for
    /// commands that name their own target template (`load_template`, `unload
    /// <rrid>`) and must never auto-fan-out.
    Single,
}

/// An executable mtui command.
///
/// Concrete commands implement [`name`](Command::name) and the abstract
/// [`call`](Command::call) body; the rest default. The engine dispatches a
/// parsed line to the matching command and awaits [`run`](Command::run).
#[async_trait]
pub trait Command: Send + Sync {
    /// The user-facing command string (the registry key), e.g. `"run"`.
    fn name(&self) -> &'static str;

    /// Alternate names the command also answers to. Empty by default.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// A one-line description of the command, or `None` if undocumented.
    ///
    /// `help` groups `Some(..)` under "Documented commands" and `None` under
    /// "Undocumented commands"; it also feeds MCP tool descriptions.
    fn about(&self) -> Option<&'static str> {
        None
    }

    /// The fan-out scope policy. [`Scope::Active`] by default.
    fn scope(&self) -> Scope {
        Scope::Active
    }

    /// Whether this command must dispatch against the **canonical** session
    /// rather than a [`fork_for_call`](crate::Session::fork_for_call), because it
    /// mutates state the fork clones by value (`config`) or owns outright (the
    /// [`TemplateRegistry`](crate::TemplateRegistry) *structure* — loading,
    /// replacing or removing an entry, or re-pointing the active template).
    ///
    /// `false` by default; `load_template`, `unload`, `switch`, and `regenerate`
    /// override it. It forces the headless MCP dispatch gate
    /// ([`McpSession::command_lock`](../../mtui_mcp/session/struct.McpSession.html))
    /// onto the **exclusive** arm even at a single template, so the mutation
    /// lands on the canonical session rather than a discarded per-call fork. A
    /// command that only mutates an already-loaded report's *content* may run on
    /// a fork: those mutations reach the shared report through the entry lock.
    fn requires_canonical_session(&self) -> bool {
        false
    }

    /// Whether the fan-out driver may skip a resolved template that has no
    /// connected hosts (when the invocation named no `-t` host).
    ///
    /// `true` by default: a host-action command (`run`, `reboot`, …) has nothing
    /// to do on a host-less template. A command whose work does not require
    /// connected hosts — notably `export`, which under `Auto`/`Kernel` sources
    /// its data from openQA — overrides it to `false` so it is dispatched at
    /// zero hosts and applies its own per-template rule.
    fn skip_hostless_templates(&self) -> bool {
        true
    }

    /// Contributes this command's arguments to its `clap` subcommand.
    ///
    /// Default is the identity: a command with no arguments. `REMAINDER`,
    /// no-exit-on-error and per-command `--help` come from the shared base
    /// parser in [`command_parser`](crate::engine::command_parser); this is the
    /// hook it extends.
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd
    }

    /// Tab-completion candidates for the current input. Empty by default.
    ///
    /// `text` is the token being completed and `line` the whole input line; the
    /// readline-style `begidx`/`endidx` are supplied by the reedline completer.
    fn complete(&self, _session: &Session, _text: &str, _line: &str) -> Vec<String> {
        Vec::new()
    }

    /// The command body, run once per resolved template.
    ///
    /// [`run`](Self::run) has already pointed `session`'s active template at the
    /// template being acted on, so `session.metadata()` / `session.targets()`
    /// reflect it.
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult;

    /// Drives [`call`](Self::call) across the resolved templates.
    ///
    /// Single-template resolution calls [`call`](Self::call) directly so the
    /// error contract is unchanged (errors propagate). Beyond one, each template
    /// gets a banner and its own boundary: failures are collected, the loop
    /// continues, and [`CommandError::FanOut`] is returned if any failed. A
    /// host-less template is skipped up front when the invocation named no `-t`
    /// hosts; every template skipped means the command ran nowhere and yields
    /// [`CommandError::NoRefhostsDefined`].
    ///
    /// Cancellation (MCP `job_cancel`): the driver is the seam's chokepoint. It
    /// bails with [`CommandError::Cancelled`] before dispatching, and re-checks
    /// between templates so a cancelled fan-out stops at the next boundary. A
    /// cancel arriving *mid*-`call` is observed only if the body opts in
    /// ([`Session::cancel_requested`]); otherwise the MCP job layer hard-aborts
    /// after its grace period. A body's own [`CommandError::Cancelled`] *is* the
    /// cancel — break at the boundary keeping the flow's detail, never collect
    /// it as a template failure. A real failure banked before the cancel
    /// outranks it and still surfaces as the [`CommandError::FanOut`] aggregate,
    /// with the stop riding along as that aggregate's `stop` note, so it neither
    /// pads the failure list nor leaves never-reached templates looking clean.
    async fn run(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        session.check_cancelled()?;
        let resolved = resolve_templates(self.scope(), session, args)?;

        if resolved.len() <= 1 {
            let restore = session.templates.active_rrid().map(str::to_owned);
            // Install this call's active handle (the entry's lock). An empty
            // RRID (empty session) clears the guard so `metadata()` falls back
            // to the null report. `activate` drops the prior guard first, so a
            // registry-mutating command (`load_template`) can re-point/re-lock
            // the active entry from inside `call` without self-deadlocking.
            let target_rrid = resolved.first().map_or("", String::as_str);
            if !session.activate(target_rrid) {
                log_activate_failure(self.name(), target_rrid);
            }
            let out = self.call(session, args).await;
            restore_active(session, restore);
            return out;
        }

        // A `-t`-taking command invoked without explicit hosts applies
        // opportunistically, so a host-less template is skipped rather than
        // failing the fan-out; explicitly named hosts must keep failing loudly.
        // `try_get_many` separates "never declared `-t`" (`Err`) from "declared
        // but unset" (`Ok(None)`) and "named hosts" (`Ok(Some)`).
        let hosts = args.try_get_many::<String>("hosts");
        let declares_hosts = hosts.is_ok();
        let named_hosts = hosts.ok().flatten().is_some_and(|mut v| v.next().is_some());
        let skippable = declares_hosts && !named_hosts && self.skip_hostless_templates();

        let restore = session.templates.active_rrid().map(str::to_owned);
        // `is_hostless` locks the entry it inspects, so a guard still held on it
        // would make that entry read as skippable. Each iteration re-activates.
        session.release_active_guard();
        let mut failures: Vec<(String, CommandError)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        let mut cancelled = false;
        // Templates whose body returned `Ok`, counted — never derived. Deriving
        // it (`resolved` minus `skipped` minus `failures`) counted every
        // never-reached template as done, reporting "stopped after M of M" for a
        // fan-out that stopped at the first.
        let mut completed: Vec<&str> = Vec::new();
        // The flow's own verdict detail when the break came from a body-level
        // cancel: `CommandError::Cancelled` promises the payload names what the
        // flow managed to do, so it is preserved rather than flattened.
        let mut body_stop: Option<(String, String)> = None;
        for rrid in &resolved {
            // Template boundary = cancellation checkpoint.
            if session.cancel_requested() {
                cancelled = true;
                break;
            }
            let is_empty = session.is_hostless(rrid);
            if skippable && is_empty {
                tracing::warn!(command = self.name(), rrid = %rrid, "skipped: no connected hosts");
                skipped.push(rrid.clone());
                continue;
            }
            if !session.activate(rrid) {
                log_activate_failure(self.name(), rrid);
            }
            session.display.template_banner(rrid);
            match self.call(session, args).await {
                Ok(()) => completed.push(rrid.as_str()),
                // A body stopped at its own checkpoint
                // (`commands::perform::map_flow_error`) *is* the cancel verdict:
                // pushing it into `failures` would let a cancel impersonate a
                // broken template and surface as a `FanOut` aggregate.
                Err(CommandError::Cancelled(detail)) => {
                    tracing::info!(command = self.name(), rrid = %rrid, "template body cancelled");
                    body_stop = Some((rrid.clone(), detail));
                    cancelled = true;
                    break;
                }
                Err(exc) => {
                    tracing::error!(command = self.name(), rrid = %rrid, error = %exc, "command failed");
                    failures.push((rrid.clone(), exc));
                }
            }
        }

        restore_active(session, restore);
        // How far the fan-out got, in one shape for both verdicts: the
        // `Cancelled` payload when the stop *is* the verdict, the `FanOut`
        // aggregate's `stop` note when a real failure outranks it. Without it a
        // caller told only "template X failed" would read the templates the
        // break never reached as having run clean.
        let stop_summary = || {
            let mut msg = format!(
                "stopped after {} of {} templates",
                completed.len(),
                resolved.len()
            );
            if let Some((rrid, detail)) = &body_stop
                && !detail.is_empty()
            {
                msg.push_str(&format!("; {rrid}: {detail}"));
            }
            msg
        };

        if cancelled && failures.is_empty() {
            // Only when nothing failed: a real per-template failure outranks the
            // stop and falls through to the `FanOut` aggregate below, because
            // burying a broken template behind a bare "cancelled" is the one
            // thing the caller must not be told.
            tracing::info!(command = self.name(), "fan-out cancelled");
            return Err(CommandError::Cancelled(stop_summary()));
        }

        if !completed.is_empty() {
            tracing::info!(command = self.name(), succeeded = %completed.join(", "));
        }
        if !skipped.is_empty() {
            tracing::info!(command = self.name(), skipped = %skipped.join(", "), "no connected host");
        }
        if !failures.is_empty() {
            return Err(CommandError::FanOut {
                failures,
                stop: cancelled.then(stop_summary),
            });
        }
        if !skipped.is_empty() && completed.is_empty() {
            // Executed on nothing: an error, never a silent success.
            return Err(CommandError::NoRefhostsDefined);
        }
        Ok(())
    }
}

/// Reports a dispatch that lost [`Session::activate`]'s `try_lock_owned` race
/// on a resolved, *loaded* rrid and will run against the fallback null report.
///
/// An empty rrid is the legitimate nothing-loaded case and stays silent. The
/// null's `report_wd()` now errors rather than resolving to the process cwd
/// (#524), so a path-taking body refuses instead of acting there — but a body
/// that only reads hosts/metadata still answers about nothing, hence the log.
fn log_activate_failure(command: &'static str, rrid: &str) {
    if rrid.is_empty() {
        return;
    }
    tracing::error!(
        command,
        rrid = %rrid,
        "activate failed: dispatching against the fallback null report"
    );
}

/// Restores the active-template pointer (and its per-call handle) after
/// dispatch.
///
/// A prior active template is re-activated. When nothing was active before, the
/// guard is refreshed onto whatever the call left active, so a `load_template`
/// that added and activated a brand-new template keeps it active.
fn restore_active(session: &mut Session, restore: Option<String>) {
    match restore {
        Some(rrid) => {
            if !session.activate(&rrid) {
                // Prior active template gone (e.g. `unload`d).
                session.refresh_active_guard();
            }
        }
        None => session.refresh_active_guard(),
    }
}

/// Returns the ordered RRIDs this invocation should act on.
///
/// An empty session resolves to a single empty-RRID entry (the active null
/// report), so `run` takes the single-call fast path.
fn resolve_templates(
    scope: Scope,
    session: &Session,
    args: &ArgMatches,
) -> Result<Vec<String>, CommandError> {
    if let Some(rrid) = arg_str(args, "template") {
        if session.templates.contains(&rrid) {
            return Ok(vec![rrid]);
        }
        return Err(CommandError::TemplateNotLoaded(rrid));
    }

    let active = || vec![session.templates.active_rrid().unwrap_or("").to_owned()];

    // Self-targeting single-shot commands run exactly once, never fanned out.
    if scope == Scope::Single {
        return Ok(active());
    }

    // Every loaded template, falling back to the active entry when none is.
    let all_templates = arg_flag(args, "all_templates");
    if all_templates || scope == Scope::Fanout {
        let all = session.templates.rrids();
        return Ok(if all.is_empty() { active() } else { all });
    }

    // Headless with several loaded: no interactive `switch`, so the active
    // pointer is unaddressable state — fan out rather than silently pick one.
    if !session.is_repl && session.templates.len() > 1 {
        return Ok(session.templates.rrids());
    }

    Ok(active())
}

/// Resolves the ordered *real* RRIDs a `command`/`argv` invocation would act on,
/// for out-of-crate callers that must know the target templates *before*
/// dispatch (the MCP per-template lock gate).
///
/// Runs the same `resolve_templates` logic `run` uses, then drops the empty-RRID
/// null-report sentinel so the caller sees only genuinely-loaded templates.
///
/// `None` means the argv does not parse here, or resolves only to the null
/// report. The caller treats that (and the multi-RRID case) as "take the
/// registry gate exclusively". Never errors: a `-T <unloaded-rrid>` yields
/// `None` so the caller serialises conservatively, and the real error surfaces
/// later at dispatch.
#[must_use]
pub fn resolve_command_rrids(
    command: &dyn Command,
    session: &Session,
    argv: &[String],
) -> Option<Vec<String>> {
    let parser = crate::engine::command_parser(command);
    let matches = parser.try_get_matches_from(argv).ok()?;
    let resolved = resolve_templates(command.scope(), session, &matches).ok()?;
    let real: Vec<String> = resolved.into_iter().filter(|r| !r.is_empty()).collect();
    if real.is_empty() { None } else { Some(real) }
}

/// Reads an optional string argument, tolerating a subcommand that never
/// declared it.
fn arg_str(args: &ArgMatches, id: &str) -> Option<String> {
    args.try_get_one::<String>(id).ok().flatten().cloned()
}

/// Reads a boolean flag, tolerating a subcommand that never declared it.
fn arg_flag(args: &ArgMatches, id: &str) -> bool {
    args.try_get_one::<bool>(id)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::commands::testkit::{log_capture, session_with_hosts};

    const RRID: &str = "SUSE:Maintenance:1:1";

    /// A single-call probe with no side effects: only `run`'s activation
    /// handling is under test here, not the body it drives.
    struct NoopSingle;

    #[async_trait]
    impl Command for NoopSingle {
        fn name(&self) -> &'static str {
            "noop_probe"
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// The fan-out counterpart of [`NoopSingle`].
    struct NoopFanout;

    #[async_trait]
    impl Command for NoopFanout {
        fn name(&self) -> &'static str {
            "noop_fanout_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Fanout
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// A fork racing the canonical session for the same entry (mechanism 2 of
    /// [`Session::fork_for_call`]'s invariant note makes this unreachable via
    /// MCP today, but not by construction): `activate` fails and `run` used to
    /// silently dispatch against the fork's own discarded null report (#478).
    #[tokio::test]
    async fn fork_activate_failure_logs_error_for_non_empty_rrid() {
        // `session_with_hosts` already calls `activate(RRID)`, so the
        // canonical session holds the entry's lock when the fork is built.
        let (session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        let display = crate::display::CommandPromptDisplay::with_sink(
            Box::new(Vec::new()),
            crate::display::ColorMode::Never,
        );
        let mut fork = session.fork_for_call(display);

        let cmd = NoopSingle;
        let parser = crate::engine::command_parser(&cmd);
        let args = parser
            .try_get_matches_from(["-T", RRID])
            .expect("argv should parse");

        log_capture::start();
        cmd.run(&mut fork, &args)
            .await
            .expect("the body still runs, against the fork's fallback null");
        let logged = log_capture::take();

        assert!(
            logged.iter().any(|c| c.message.as_deref()
                == Some("activate failed: dispatching against the fallback null report")),
            "a lost race on a non-empty rrid must log at ERROR; got: {:?}",
            logged
                .iter()
                .filter_map(|c| c.message.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The legitimate empty-session case (nothing loaded) must stay silent:
    /// only a *non-empty* rrid losing the race is diagnosable.
    #[tokio::test]
    async fn empty_session_activate_logs_nothing() {
        use crate::commands::testkit::empty_session;

        let (mut session, _buf) = empty_session();
        let cmd = NoopSingle;
        let args = crate::commands::testkit::matches(&cmd, &[]);

        log_capture::start();
        cmd.run(&mut session, &args)
            .await
            .expect("an empty session still dispatches against the null report");
        let logged = log_capture::take();

        assert!(
            logged.iter().all(|c| c.message.is_none()),
            "an empty rrid is the legitimate nothing-loaded case, not a lost race; got: {:?}",
            logged
                .iter()
                .filter_map(|c| c.message.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The fan-out dispatch site has the same contract as the single-template
    /// one, and must name the template that lost: an entry locked from outside
    /// diverts only *its* iteration onto the null report, while its siblings
    /// activate normally (#524).
    #[tokio::test]
    async fn fanout_activate_failure_logs_error_for_the_losing_template() {
        const OTHER: &str = "SUSE:Maintenance:2:2";

        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session
            .templates
            .add(crate::commands::testkit::fake_report(OTHER, &["h2"], "ok"));
        // `run` releases the guard itself before the loop; do it here too so the
        // entry can be claimed from outside the session.
        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let cmd = NoopFanout;
        let args = crate::commands::testkit::matches(&cmd, &[]);

        log_capture::start();
        cmd.run(&mut session, &args)
            .await
            .expect("both templates still dispatch");
        let logged = log_capture::take();

        let losers: Vec<String> = logged
            .iter()
            .filter(|c| {
                c.message.as_deref()
                    == Some("activate failed: dispatching against the fallback null report")
            })
            .filter_map(|c| c.rrid.clone())
            .collect();
        assert_eq!(
            losers,
            vec![RRID.to_owned()],
            "exactly the locked template must be reported"
        );
    }
}
