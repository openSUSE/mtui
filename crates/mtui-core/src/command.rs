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

    /// The fan-out scope policy. [`Scope::Active`] by default.
    fn scope(&self) -> Scope {
        Scope::Active
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
            // Empty session: the sole "resolved" entry is the active null
            // report; running against it preserves the historical single-call
            // dispatch. A named RRID is guaranteed loaded by resolve_templates.
            if let Some(rrid) = resolved.first() {
                session.templates.set_active(rrid);
            }
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
        let skippable = declares_hosts && !named_hosts;

        let restore = session.templates.active_rrid().map(str::to_owned);
        let mut failures: Vec<(String, CommandError)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for rrid in &resolved {
            let is_empty = session
                .templates
                .get(rrid)
                .map(|r| r.base().targets.is_empty())
                .unwrap_or(true);
            if skippable && is_empty {
                tracing::warn!(command = self.name(), rrid = %rrid, "skipped: no connected hosts");
                skipped.push(rrid.clone());
                continue;
            }
            session.templates.set_active(rrid);
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

/// Restores the active-template pointer after fan-out (leaves it as-is if the
/// restored RRID is no longer loaded).
fn restore_active(session: &mut Session, restore: Option<String>) {
    if let Some(rrid) = restore {
        session.templates.set_active(&rrid);
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
    if !session.interactive && session.templates.len() > 1 {
        return Ok(session.templates.rrids());
    }

    Ok(active())
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
