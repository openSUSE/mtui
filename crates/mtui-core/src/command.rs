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
use crate::session::{Activation, Session};

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

    /// Whether the body reads the report [`run`](Self::run) resolved for it.
    ///
    /// `true` by default: a body that reads it must be refused when the entry
    /// cannot be claimed, never answered off the null sentinel (#524). Only a
    /// [`Scope::Single`] command invoked without `-T` is exempt from that
    /// refusal, and only when this is `false` — it addressed no template
    /// (`resolve_templates` handed it whatever was active) *and* ignores what
    /// it was handed.
    ///
    /// Reading *a* report is not the test: `load_template` prints the host
    /// count of the template it just loaded, never the one it was handed.
    fn reads_resolved_report(&self) -> bool {
        true
    }

    /// Whether this *invocation* must dispatch against the **canonical** session
    /// rather than a [`fork_for_call`](crate::Session::fork_for_call), because it
    /// mutates state the fork clones by value (`config`) or owns outright (the
    /// [`TemplateRegistry`](crate::TemplateRegistry) *structure* — loading,
    /// replacing or removing an entry, or re-pointing the active template).
    ///
    /// Per-**invocation**, not per-command, hence `argv`: several MCP tools may be
    /// synthesised from one registry command and differ only in argv
    /// (`config_show` and `config_set` are both `config`), so a whole-command
    /// answer would put the read-only ones on the exclusive gate too.
    ///
    /// `false` by default; `load_template`, `unload`, `switch` and `regenerate`
    /// override it unconditionally, `config` for its `set` subcommand. It forces
    /// the headless MCP dispatch gate
    /// ([`McpSession::command_lock`](../../mtui_mcp/session/struct.McpSession.html))
    /// onto the **exclusive** arm even at a single template, so the mutation
    /// lands on the canonical session rather than a discarded per-call fork. A
    /// command that only mutates an already-loaded report's *content* may run on
    /// a fork: those mutations reach the shared report through the entry lock.
    fn requires_canonical_session(&self, _argv: &[String]) -> bool {
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

    /// Whether this command's body deliberately repoints the active template,
    /// so [`run`](Self::run) must keep the move instead of restoring the
    /// pre-dispatch pointer.
    ///
    /// `false` by default; only honoured on the single-template path, which is
    /// where a [`Scope::Single`] command always lands. `switch`, whose entire
    /// point is to leave the pointer moved, and `load_template`/`regenerate`,
    /// whose loaded RRID must become active, override it unconditionally.
    fn repoints_active(&self) -> bool {
        false
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
    /// error contract is unchanged (errors propagate), then restores the
    /// pre-dispatch active pointer — unless [`Command::repoints_active`] says the
    /// body's move is deliberate, in which case the new pointer stands. Beyond
    /// one, each template gets a banner and its own boundary: failures are
    /// collected, the loop continues, and [`CommandError::FanOut`] is returned
    /// if any failed. A host-less template is skipped up front when the
    /// invocation named no `-t` hosts; every template skipped means the command
    /// ran nowhere and yields [`CommandError::NoRefhostsDefined`].
    ///
    /// A template whose report entry cannot be claimed is refused
    /// ([`CommandError::TemplateBusy`]) rather than dispatched against the null
    /// sentinel; under fan-out that refusal is that template's failure alone,
    /// and a contended entry is never mistaken for a host-less one and skipped.
    /// The exception is a [`Scope::Single`] command invoked without `-T` that
    /// declares [`reads_resolved_report`](Self::reads_resolved_report) `false`:
    /// it addressed no template and ignores the one it was handed, so its claim
    /// stays best-effort.
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
            // `repoints_active` opts out of the restore below: the body's move
            // (`switch`, a loading `load_template`/`regenerate`) is the point of
            // the call, not a side effect to undo.
            let restore = (!self.repoints_active())
                .then(|| session.templates.active_rrid().map(str::to_owned))
                .flatten();
            // Install this call's active handle (the entry's lock). An empty
            // RRID (empty session) clears the guard so `metadata()` falls back
            // to the null report. `activate` drops the prior guard first, so a
            // registry-mutating command (`load_template`) can re-point/re-lock
            // the active entry from inside `call` without self-deadlocking.
            let target_rrid = resolved.first().map_or("", String::as_str);
            // A `Scope::Single` command with no explicit `-T` addressed no
            // template at all: `resolve_templates` handed back whatever is
            // active. Refusing it because some *other* dispatch holds that
            // entry fails a command over a template the operator never named.
            // The exemption is per-command, not per-scope — `regenerate` is
            // also `Scope::Single` but does read what it was handed, and
            // reading that off the null sentinel is #524 itself.
            //
            // `claim` runs either way — it is what points the session at the
            // template. Only the *refusal* is conditional, so the order here
            // matters and must not be flipped.
            let exempt = self.scope() == Scope::Single
                && !self.reads_resolved_report()
                && arg_str(args, "template").is_none();
            if let Err(exc) = claim(self.name(), session, target_rrid)
                && !exempt
            {
                restore_active(session, restore);
                return Err(exc);
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
        // would make that entry unreadable. Each iteration re-activates. This
        // sheds only *our* guard; a foreign holder is the `None` case below.
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
            // `None` = held elsewhere, host set unreadable. Only a *readable*
            // empty entry is skippable: treating the contended one as hostless
            // skipped it with this warning and exited 0, which is the #524
            // symptom, reached before `claim` below could refuse it.
            if skippable && session.is_hostless(rrid) == Some(true) {
                tracing::warn!(command = self.name(), rrid = %rrid, "skipped: no connected hosts");
                skipped.push(rrid.clone());
                continue;
            }
            // A template that cannot be claimed is this fan-out's failure, not
            // the whole command's: bank it and keep going, like any other
            // per-template error.
            if let Err(exc) = claim(self.name(), session, rrid) {
                failures.push((rrid.clone(), exc));
                continue;
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

/// Points the session at `rrid` for this dispatch, or refuses.
///
/// An empty rrid is the legitimate nothing-loaded session and dispatches on the
/// sentinel as before. Anything else that fails to claim the entry is the #524
/// race, and running anyway is what made it silent: `report_wd()` errors on the
/// sentinel now, but a body that only reads hosts/metadata (`list_hosts`,
/// `list_packages`) would still answer about nothing, with exit 0. Refuse
/// instead, and name the template — the ERROR log alone never reaches the MCP
/// caller, whose reply is a per-call display capture.
fn claim(command: &'static str, session: &mut Session, rrid: &str) -> CommandResult {
    match session.activate(rrid) {
        Activation::Active | Activation::Empty => Ok(()),
        Activation::Busy => {
            tracing::error!(command, rrid = %rrid, "activate lost the entry race");
            Err(CommandError::TemplateBusy(rrid.to_owned()))
        }
        // Resolution already checked the registry, so this is an unload that
        // landed in between — not a user naming a bad rrid.
        Activation::NotLoaded => {
            tracing::error!(command, rrid = %rrid, "activate found no entry");
            Err(CommandError::TemplateNotLoaded(rrid.to_owned()))
        }
    }
}

/// Restores the active-template pointer (and its per-call handle) after
/// dispatch.
///
/// A prior active template is re-activated. `restore` is `None` both when
/// nothing was active before *and* when [`Command::repoints_active`] opted the
/// call out of the restore — either way the guard is refreshed onto whatever
/// the call left active, so a body that deliberately moved the pointer
/// (`switch`, a loading `load_template`/`regenerate`) keeps the move.
fn restore_active(session: &mut Session, restore: Option<String>) {
    match restore {
        Some(rrid) => {
            if !session.activate(&rrid).is_active() {
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
    use std::sync::{Arc, Mutex};

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

    /// The fan-out counterpart of [`NoopSingle`], recording the rrid each
    /// dispatch was pointed at so "which templates actually ran" is observable.
    struct RecordingFanout(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Command for RecordingFanout {
        fn name(&self) -> &'static str {
            "recording_fanout_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Fanout
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            let rrid = session.templates.active_rrid().unwrap_or("").to_owned();
            self.0.lock().expect("probe mutex").push(rrid);
            Ok(())
        }
    }

    /// [`RecordingFanout`] plus the `-t` arg, which is what makes a template
    /// *skippable*: `skip_hostless_templates` is true by default, so a real
    /// host-action command (`update`, `run`, `reboot`, …) takes the skip path
    /// the plain probe never reaches.
    struct SkippableFanout(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Command for SkippableFanout {
        fn name(&self) -> &'static str {
            "skippable_fanout_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Fanout
        }
        fn configure(&self, cmd: clap::Command) -> clap::Command {
            crate::commands::support::add_hosts_arg(cmd)
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            let rrid = session.templates.active_rrid().unwrap_or("").to_owned();
            self.0.lock().expect("probe mutex").push(rrid);
            Ok(())
        }
    }

    /// A [`Scope::Single`] probe that resolves to whatever is active and
    /// ignores it, like `unload <rrid>` / `config` / `help`.
    struct NoopSingleScope;

    #[async_trait]
    impl Command for NoopSingleScope {
        fn name(&self) -> &'static str {
            "noop_single_scope_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn reads_resolved_report(&self) -> bool {
            false
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// The other kind of [`Scope::Single`]: it reads the report it was handed,
    /// like `regenerate` (`require_update`). Answering off the null sentinel is
    /// #524, so the default `reads_resolved_report` stands and the refusal
    /// applies.
    struct ReportReadingSingleScope(Arc<Mutex<Vec<usize>>>);

    #[async_trait]
    impl Command for ReportReadingSingleScope {
        fn name(&self) -> &'static str {
            "report_reading_single_scope_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            self.0
                .lock()
                .expect("probe mutex")
                .push(session.targets().len());
            Ok(())
        }
    }

    /// A fork racing the canonical session for the same entry (mechanism 2 of
    /// [`Session::fork_for_call`]'s invariant note makes this unreachable via
    /// MCP today, but not by construction): `activate` fails and `run` used to
    /// silently dispatch against the fork's own discarded null report (#478),
    /// then to log and dispatch anyway (#524). It now refuses.
    #[tokio::test]
    async fn fork_activate_failure_refuses_for_non_empty_rrid() {
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
        let err = cmd
            .run(&mut fork, &args)
            .await
            .expect_err("a lost race must refuse, not answer from the fallback null");
        let logged = log_capture::take();

        assert!(
            matches!(err, CommandError::TemplateBusy(ref r) if r == RRID),
            "the refusal must name the contended template; got: {err:?}"
        );
        assert!(
            logged
                .iter()
                .any(|c| c.message.as_deref() == Some("activate lost the entry race")),
            "a lost race on a non-empty rrid must also log at ERROR; got: {:?}",
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
    /// one, and the failure is *per template*: the locked entry is banked as
    /// that template's error while its siblings run normally (#524).
    #[tokio::test]
    async fn fanout_activate_failure_fails_only_the_losing_template() {
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

        let seen = Arc::new(Mutex::new(Vec::new()));
        let cmd = RecordingFanout(Arc::clone(&seen));
        let args = crate::commands::testkit::matches(&cmd, &[]);

        log_capture::start();
        let err = cmd
            .run(&mut session, &args)
            .await
            .expect_err("the locked template must fail the fan-out");
        let logged = log_capture::take();

        // The operator-side diagnostic still names which template lost, which
        // the aggregate alone does not distinguish from a body failure.
        let losers_logged: Vec<String> = logged
            .iter()
            .filter(|c| c.message.as_deref() == Some("activate lost the entry race"))
            .filter_map(|c| c.rrid.clone())
            .collect();
        assert_eq!(losers_logged, vec![RRID.to_owned()]);

        let CommandError::FanOut { failures, .. } = &err else {
            panic!("expected a per-template aggregate; got: {err:?}");
        };
        let losers: Vec<&str> = failures.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(losers, vec![RRID], "exactly the locked template must fail");
        assert!(
            matches!(failures[0].1, CommandError::TemplateBusy(_)),
            "the banked error must be the refusal: {:?}",
            failures[0].1
        );
        // The body ran for the sibling and *only* the sibling — a refusal that
        // also skipped the healthy template would be a worse bug than the one
        // it fixes.
        assert_eq!(
            seen.lock().expect("probe mutex").as_slice(),
            [OTHER.to_owned()]
        );
    }

    /// The refusal is reached *before* the host-less skip, not after it.
    ///
    /// `is_hostless` inspects the entry with `try_lock`, so a contended one used
    /// to read as empty and take the skip path — `skipped: no connected hosts`,
    /// `continue`, and (with a healthy sibling to keep `completed` non-empty)
    /// **exit 0**. That is #524's symptom on the default fan-out path, which is
    /// every host-action command, so it has to be a refusal like any other.
    #[tokio::test]
    async fn fanout_contended_template_fails_rather_than_reading_as_hostless() {
        const OTHER: &str = "SUSE:Maintenance:2:2";

        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session
            .templates
            .add(crate::commands::testkit::fake_report(OTHER, &["h2"], "ok"));
        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let cmd = SkippableFanout(Arc::clone(&seen));
        // No `-t`: this is exactly the invocation shape that enables skipping.
        let args = crate::commands::testkit::matches(&cmd, &[]);
        assert!(
            cmd.skip_hostless_templates(),
            "anti-vacuity: the skip path must be live for this test to mean anything"
        );

        let err = cmd
            .run(&mut session, &args)
            .await
            .expect_err("a contended template must fail, not be skipped as host-less");

        let CommandError::FanOut { failures, .. } = &err else {
            panic!("expected a per-template aggregate; got: {err:?}");
        };
        assert!(
            matches!(&failures[..], [(r, CommandError::TemplateBusy(_))] if r == RRID),
            "the contended template must be the one banked failure; got: {failures:?}"
        );
        assert_eq!(
            seen.lock().expect("probe mutex").as_slice(),
            [OTHER.to_owned()],
            "the healthy sibling must still run"
        );
    }

    /// A `Scope::Single` command that named no template must not be refused
    /// because something holds the *active* entry.
    ///
    /// `resolve_templates` hands `Scope::Single` the active rrid as a fallback,
    /// but `unload <rrid>` / `load_template` / `config` / `help` never read that
    /// report. Refusing them on a contended active entry fails a command over a
    /// template the operator never mentioned — worse than the bug being fixed,
    /// because it is reachable without any race of the caller's own making.
    #[tokio::test]
    async fn single_scope_without_an_explicit_template_is_not_refused() {
        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let cmd = NoopSingleScope;
        let args = crate::commands::testkit::matches(&cmd, &[]);

        cmd.run(&mut session, &args).await.expect(
            "a Single-scope command that addressed no template must survive a hold on the \
             active entry",
        );
    }

    /// The counterpart: the same scope *does* refuse once the caller names the
    /// contended template with `-T`, so the carve-out above is scoped to the
    /// fallback and is not a blanket exemption.
    #[tokio::test]
    async fn single_scope_with_an_explicit_template_still_refuses() {
        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let cmd = NoopSingleScope;
        let parser = crate::engine::command_parser(&cmd);
        let args = parser
            .try_get_matches_from(["-T", RRID])
            .expect("argv should parse");

        let err = cmd
            .run(&mut session, &args)
            .await
            .expect_err("an explicitly addressed contended template must still refuse");
        assert!(
            matches!(err, CommandError::TemplateBusy(ref r) if r == RRID),
            "got: {err:?}"
        );
    }

    /// The exemption is per-command, not per-scope: a `Scope::Single` command
    /// that reads what it was handed is refused even bare. Deriving it from the
    /// scope alone let `regenerate` answer `Metadata not loaded` off the null
    /// sentinel.
    #[tokio::test]
    async fn single_scope_reading_the_report_is_refused_even_without_a_template() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cmd = ReportReadingSingleScope(Arc::clone(&seen));

        // Anti-vacuity: uncontended, the body sees the template's real host.
        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        let args = crate::commands::testkit::matches(&cmd, &[]);
        cmd.run(&mut session, &args)
            .await
            .expect("uncontended dispatch should succeed");
        assert_eq!(*seen.lock().expect("probe mutex"), vec![1]);

        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let err = cmd.run(&mut session, &args).await.expect_err(
            "a Single-scope command that reads the resolved report must refuse a contended entry",
        );
        assert!(
            matches!(err, CommandError::TemplateBusy(ref r) if r == RRID),
            "got: {err:?}"
        );
        assert_eq!(
            *seen.lock().expect("probe mutex"),
            vec![1],
            "the body must not have run a second time and reported the sentinel's zero hosts"
        );
    }
}
