//! The `Command` trait, its fan-out [`Scope`], and the template fan-out engine.
//!
//! Port of upstream `mtui.commands._command.Command`. Every command implements
//! [`Command`] and is discovered through the registry (P5.2); the REPL, tab
//! completion, and the MCP tool synthesiser all iterate that one registry.
//!
//! A command supplies its abstract body in [`call`](Command::call); the provided
//! [`run`](Command::run) drives that body across the templates the invocation
//! resolves to, faithfully porting upstream `_resolve_templates` + `run`:
//!
//! * `-T/--template RRID` scopes to exactly one loaded template.
//! * `--all-templates` (or [`Scope::Fanout`]) fans out across every template.
//! * [`Scope::Single`] always runs exactly once (self-targeting commands like
//!   `unload <rrid>`), never auto-fanned-out.
//! * Otherwise the active template — except headlessly (MCP, `interactive =
//!   false`) with more than one loaded, where there is no addressable active
//!   pointer, so the call fans out.
//!
//! Fan-out aggregates per-template failures: each template gets a banner and its
//! own error boundary, a host-less template is skipped when no `-t` host was
//! named, and a [`CommandError::FanOut`] is raised afterwards if any template
//! failed. If every template was skipped the command ran nowhere, which is a
//! [`CommandError::NoRefhostsDefined`], not a silent success.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Fan-out scope policy for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Run once against the active template. Under MCP with several templates
    /// loaded this defaults to fan-out (there is no addressable active pointer).
    /// The safe default.
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
/// [`call`](Command::call) body; the rest have sensible defaults. The engine
/// (P5.2) dispatches a parsed line to the matching command and awaits
/// [`run`](Command::run).
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
    /// The Rust replacement for upstream's docstring convention: `help` groups
    /// commands returning `Some(..)` under "Documented commands" and those
    /// returning `None` under "Undocumented commands". Defaults to `None`;
    /// commands opt in by overriding it. (It also feeds MCP tool descriptions in
    /// Phase 7.)
    fn about(&self) -> Option<&'static str> {
        None
    }

    /// The fan-out scope policy. [`Scope::Active`] by default.
    fn scope(&self) -> Scope {
        Scope::Active
    }

    /// Whether this command's body mutates the [`TemplateRegistry`] *structure*
    /// (loads/replaces/removes an entry or re-points the active template), as
    /// opposed to only mutating an already-loaded report's *content*.
    ///
    /// `false` by default; `load_template`, `unload`, `switch`, and `regenerate`
    /// override it to `true`. The headless MCP dispatch gate
    /// ([`McpSession::command_lock`](../../mtui_mcp/session/struct.McpSession.html))
    /// forces such a command onto the **exclusive** registry gate even when it
    /// resolves to a single template, so its structural mutation lands on the
    /// canonical session rather than on a discarded per-call fork
    /// (`mtui-rs-f36r`, steps 4-5). A content-only per-RRID command may run on a
    /// fork because its mutations reach the shared report through the entry lock.
    fn mutates_registry(&self) -> bool {
        false
    }

    /// Whether the fan-out driver may skip a resolved template that has no
    /// connected hosts (when the invocation named no `-t` host).
    ///
    /// `true` by default: a host-action command (`run`, `reboot`, …) has nothing
    /// to do on a host-less template, so the driver skips it up front rather than
    /// letting the body no-op or fail. A command whose work does not strictly
    /// require connected hosts — notably `export`, which for the `Auto`/`Kernel`
    /// workflows sources its data from openQA — overrides this to `false` so it
    /// is dispatched even at zero hosts and can apply its own per-template rule.
    fn skip_hostless_templates(&self) -> bool {
        true
    }

    /// Contributes this command's arguments to its `clap` subcommand.
    ///
    /// Default is the identity: a command with no arguments. The real
    /// argparse↔clap fidelity work (`REMAINDER`, no-exit-on-error, per-command
    /// `--help`) lands in P5.4; this is the hook it fills in.
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd
    }

    /// Tab-completion candidates for the current input. Empty by default.
    ///
    /// `text` is the token being completed and `line` the whole input line;
    /// mirrors upstream `complete(state, text, line, begidx, endidx)` minus the
    /// readline index args, which the reedline completer (Phase 6) supplies.
    fn complete(&self, _session: &Session, _text: &str, _line: &str) -> Vec<String> {
        Vec::new()
    }

    /// The command body, run once per resolved template.
    ///
    /// When [`run`](Self::run) invokes this, `session`'s active template pointer
    /// has been set to the template being acted on, so `session.metadata()` /
    /// `session.targets()` reflect it. Concrete commands implement this; the
    /// fan-out driver is [`run`](Self::run).
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult;

    /// Drives [`call`](Self::call) across the resolved templates.
    ///
    /// Single-template resolution calls [`call`](Self::call) directly so the
    /// error contract is unchanged (errors propagate). With more than one
    /// resolved template, each gets a banner and its own boundary: a per-template
    /// failure is collected and the loop continues, then a
    /// [`CommandError::FanOut`] is returned if any failed. A template with no
    /// connected host is skipped up front (when the invocation named no `-t`
    /// hosts); if every template was skipped the command ran nowhere and
    /// [`CommandError::NoRefhostsDefined`] is returned.
    async fn run(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let resolved = resolve_templates(self.scope(), session, args)?;

        if resolved.len() <= 1 {
            let restore = session.templates.active_rrid().map(str::to_owned);
            // Install this call's active handle (the entry's lock). An empty
            // "resolved" entry (empty session) clears the guard so `metadata()`
            // falls back to the null report — the historical single-call
            // dispatch. A named RRID is guaranteed loaded by resolve_templates.
            //
            // `activate` drops the prior guard first, so a command that mutates
            // the registry (`load_template`) can then re-point/re-lock the active
            // entry from inside `call` without self-deadlocking on the guard.
            session.activate(resolved.first().map_or("", String::as_str));
            let out = self.call(session, args).await;
            restore_active(session, restore);
            return out;
        }

        // A host-phase command (one taking `-t`) invoked without explicit hosts
        // opportunistically applies to every loaded template; a template with no
        // connected host has nothing to act on and is skipped so it can't fail
        // the fan-out. Explicitly named hosts must keep failing loudly.
        //
        // Upstream keys this on `hasattr(args, "hosts")` (does the command
        // *declare* `-t`?) and `not args.hosts` (were any named?). In clap,
        // `try_get_many` distinguishes the two: `Err` = the arg was never
        // declared, `Ok(None)` = declared but unset, `Ok(Some)` = declared with
        // values.
        let hosts = args.try_get_many::<String>("hosts");
        let declares_hosts = hosts.is_ok();
        let named_hosts = hosts.ok().flatten().is_some_and(|mut v| v.next().is_some());
        let skippable = declares_hosts && !named_hosts && self.skip_hostless_templates();

        let restore = session.templates.active_rrid().map(str::to_owned);
        // Release any held active handle before probing entries: `is_hostless`
        // locks the entry it inspects, so a guard still held on it would make it
        // read as skippable. Each iteration re-activates its own template below.
        session.release_active_guard();
        let mut failures: Vec<(String, CommandError)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for rrid in &resolved {
            let is_empty = session.is_hostless(rrid);
            if skippable && is_empty {
                tracing::warn!(command = self.name(), rrid = %rrid, "skipped: no connected hosts");
                skipped.push(rrid.clone());
                continue;
            }
            session.activate(rrid);
            session.display.template_banner(rrid);
            if let Err(exc) = self.call(session, args).await {
                tracing::error!(command = self.name(), rrid = %rrid, error = %exc, "command failed");
                failures.push((rrid.clone(), exc));
            }
        }

        restore_active(session, restore);

        let done: std::collections::HashSet<&str> = failures
            .iter()
            .map(|(r, _)| r.as_str())
            .chain(skipped.iter().map(String::as_str))
            .collect();
        let ok: Vec<&str> = resolved
            .iter()
            .map(String::as_str)
            .filter(|r| !done.contains(r))
            .collect();
        if !ok.is_empty() {
            tracing::info!(command = self.name(), succeeded = %ok.join(", "));
        }
        if !skipped.is_empty() {
            tracing::info!(command = self.name(), skipped = %skipped.join(", "), "no connected host");
        }
        if !failures.is_empty() {
            return Err(CommandError::FanOut(failures));
        }
        if !skipped.is_empty() && ok.is_empty() {
            // Every resolved template was skipped: the command executed on
            // nothing, which must stay an error, not a silent success.
            return Err(CommandError::NoRefhostsDefined);
        }
        Ok(())
    }
}

/// Restores the active-template pointer (and its per-call handle) after
/// dispatch.
///
/// When a prior template was active it is re-activated (leaving it as-is if the
/// restored RRID is no longer loaded). When nothing was active before, the guard
/// is refreshed onto whatever the call left active — so a `load_template` that
/// added and activated a brand-new template keeps it active, matching the
/// historical `restore_active(None)` no-op followed by the new `set_active`.
fn restore_active(session: &mut Session, restore: Option<String>) {
    match restore {
        Some(rrid) => {
            if !session.activate(&rrid) {
                // The prior active template is gone (e.g. `unload`d): fall back to
                // whatever remains active in the registry.
                session.refresh_active_guard();
            }
        }
        None => session.refresh_active_guard(),
    }
}

/// Returns the ordered RRIDs this invocation should act on, porting upstream
/// `_resolve_templates`.
///
/// An empty session resolves to a single empty-RRID entry (the active null
/// report), so `run` takes the single-call fast path exactly as the historical
/// dispatch did.
fn resolve_templates(
    scope: Scope,
    session: &Session,
    args: &ArgMatches,
) -> Result<Vec<String>, CommandError> {
    // -T/--template RRID → exactly that template (must be loaded).
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

    // --all-templates or a fanout-scoped command → every loaded template
    // (falling back to the active entry when nothing is loaded).
    let all_templates = arg_flag(args, "all_templates");
    if all_templates || scope == Scope::Fanout {
        let all = session.templates.rrids();
        return Ok(if all.is_empty() { active() } else { all });
    }

    // Headless (MCP) with several templates loaded: no interactive `switch`, so
    // the active pointer is hidden, unaddressable state — default an unscoped
    // call to fan-out instead of silently picking one. The REPL keeps its
    // active-template behaviour.
    if !session.is_repl && session.templates.len() > 1 {
        return Ok(session.templates.rrids());
    }

    Ok(active())
}

/// Resolves the ordered *real* RRIDs a `command`/`argv` invocation would act on,
/// for out-of-crate callers that must know the target templates *before*
/// dispatch (the MCP per-template lock gate, `mtui-rs-76e.11`).
///
/// The public port of upstream `McpSession._resolve_job_rrids`: it builds the
/// command's clap parser, parses `argv`, and runs the identical
/// [`resolve_templates`] fan-out logic `run` uses, then drops the empty-RRID
/// null-report sentinel so the caller sees only genuinely-loaded templates.
///
/// Returns:
/// * `Some(rrids)` — one or more loaded templates this call targets;
/// * `None` — the argv does not parse here, or it resolves only to the null
///   report (nothing loaded / active is null). The caller treats `None` (and the
///   multi-RRID case) as "take the registry gate exclusively", exactly as
///   upstream falls back when resolution is not a single real template.
///
/// This never errors: a `-T <unloaded-rrid>` (which [`resolve_templates`] would
/// reject) yields `None` so the caller serialises conservatively rather than
/// surfacing a lock-layer parse error; the real error surfaces later at dispatch.
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
