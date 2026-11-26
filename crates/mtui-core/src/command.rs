//! The `Command` trait, its fan-out [`Scope`], and the template fan-out engine.
//!
//! Every command implements [`Command`] and is discovered through the registry.
//! A command supplies its body in [`call`](Command::call); the provided
//! [`run`](Command::run) drives it across the templates the invocation resolves
//! to:
//!
//! * `-T/--template RRID` — exactly that loaded template (declared only by
//!   commands that address one, see [`addresses_template`]).
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
use crate::template_scope::{SingleTemplate, canonical_template_name};

/// Fan-out scope policy for a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Run once against the active template; under MCP with several templates
    /// loaded, fan out instead.
    #[default]
    Active,
    /// Run once per loaded template. Action commands safe to repeat opt in.
    Fanout,
    /// Never implicitly fans out: with no `-T`/`--all-templates`, resolves to
    /// exactly one template via
    /// [`Session::resolve_single_template`](crate::Session::resolve_single_template),
    /// or [`CommandError::AmbiguousTemplate`] headlessly with several loaded.
    /// For destructive or remote-write commands (`update`, `downgrade`,
    /// `approve`, …).
    Explicit,
    /// Run exactly once, for commands that name their own target template
    /// (`load_template`, `unload <rrid>`). Never declares `--all-templates`;
    /// [`addresses_template`] decides `-T`.
    Single,
}

/// Whether `command` acts on the template `run` resolves for it, and so
/// declares the shared `-T/--template` flag. Derived, never overridden: it is
/// also `run`'s claim exemption, so parser and driver cannot disagree.
#[must_use]
pub fn addresses_template<C: Command + ?Sized>(command: &C) -> bool {
    !(command.scope() == Scope::Single && !command.reads_resolved_report())
}

/// An executable mtui command.
///
/// Concrete commands implement [`name`](Command::name) and
/// [`call`](Command::call); the rest default.
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
    /// `true` by default: a body that reads it is refused when the entry cannot
    /// be claimed, never answered off the null sentinel. `false` on a
    /// [`Scope::Single`] command exempts it from that refusal and drops
    /// `-T/--template` from its parser ([`addresses_template`]).
    ///
    /// Reading *a* report is not the test: `load_template` reports the template
    /// it just loaded, never the one it was handed.
    fn reads_resolved_report(&self) -> bool {
        true
    }

    /// Whether this *invocation* must dispatch against the **canonical** session
    /// rather than a [`fork_for_call`](crate::Session::fork_for_call), because it
    /// mutates state the fork clones by value (`config`) or owns outright (the
    /// [`TemplateRegistry`](crate::TemplateRegistry) structure: loading,
    /// replacing or removing an entry, or re-pointing the active template).
    ///
    /// Per-invocation, hence `argv`: `config_show` and `config_set` are both
    /// `config` but only `set` needs the exclusive gate.
    ///
    /// `false` by default. When `true`, the headless MCP dispatch gate
    /// ([`McpSession::command_lock`](../../mtui_mcp/session/struct.McpSession.html))
    /// goes exclusive even at a single template, so the mutation lands on the
    /// canonical session rather than a discarded fork. Mutating an already-loaded
    /// report's *content* may run on a fork: it reaches the shared report through
    /// the entry lock.
    fn requires_canonical_session(&self, _argv: &[String]) -> bool {
        false
    }

    /// Whether the fan-out driver may skip a resolved template that has no
    /// connected hosts (when the invocation named no `-t` host).
    ///
    /// `true` by default: a host-action command has nothing to do on a host-less
    /// template. `export` (which can source from openQA) overrides it to `false`
    /// and applies its own per-template rule.
    fn skip_hostless_templates(&self) -> bool {
        true
    }

    /// Whether the fan-out driver treats retained removed-host logs as
    /// ownership when skipping a template that owns none of the named `-t`
    /// hosts.
    ///
    /// `false` by default: a removed host is gone, so a host-action command
    /// skips the template rather than fail it with `HostNotConnected`. `show_log`
    /// answers removed hosts from their retained log and opts in.
    fn includes_detached_hosts(&self) -> bool {
        false
    }

    /// Whether this command's body deliberately repoints the active template,
    /// so [`run`](Self::run) must keep the move instead of restoring the
    /// pre-dispatch pointer.
    ///
    /// `false` by default; only honoured on the single-template path. `switch`,
    /// `load_template` and `regenerate` override it.
    fn repoints_active(&self) -> bool {
        false
    }

    /// Contributes this command's arguments to its `clap` subcommand.
    ///
    /// The identity by default. The shared base parser is
    /// [`command_parser`](crate::engine::command_parser).
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd
    }

    /// Tab-completion candidates for the current input. Empty by default.
    ///
    /// `text` is the token being completed and `line` the whole input line.
    fn complete(&self, _session: &Session, _text: &str, _line: &str) -> Vec<String> {
        Vec::new()
    }

    /// The command body, run once per resolved template.
    ///
    /// [`run`](Self::run) has already pointed `session`'s active template at the
    /// one being acted on.
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult;

    /// Drives [`call`](Self::call) across the resolved templates.
    ///
    /// A single resolved template propagates [`call`](Self::call)'s error
    /// directly, then restores the pre-dispatch active pointer unless
    /// [`Command::repoints_active`] says the move is deliberate. Beyond one, each
    /// template gets a banner and its own error boundary, and
    /// [`CommandError::FanOut`] is returned if any failed. A host-less template
    /// is skipped when no `-t` host was named; every template skipped yields
    /// [`CommandError::NoRefhostsDefined`].
    ///
    /// A template whose report entry cannot be claimed is refused
    /// ([`CommandError::TemplateBusy`]) rather than dispatched against the null
    /// sentinel; under fan-out that is that template's failure alone, and a
    /// contended entry is never mistaken for a host-less one. A command that
    /// addresses no template ([`addresses_template`]) points the session but
    /// releases the entry for the call.
    ///
    /// Cancellation: the driver bails with [`CommandError::Cancelled`] before
    /// dispatching and re-checks between templates. A cancel arriving mid-`call`
    /// is observed only if the body opts in ([`Session::cancel_requested`]). A
    /// body's own [`CommandError::Cancelled`] *is* the cancel: break keeping its
    /// detail, never collect it as a template failure. A real failure banked
    /// before the cancel outranks it and surfaces as [`CommandError::FanOut`],
    /// with the stop in that aggregate's `stop` note.
    async fn run(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        session.check_cancelled()?;
        let resolved = resolve_templates(self.name(), self.scope(), session, args)?;

        if resolved.len() <= 1 {
            let restore = (!self.repoints_active())
                .then(|| session.templates.active_rrid().map(str::to_owned))
                .flatten();
            // `activate` drops the prior guard first, so a registry-mutating
            // command can re-lock the active entry inside `call` without
            // self-deadlocking.
            let target_rrid = resolved.first().map_or("", String::as_str);
            // `claim` runs either way (it points the session at the template);
            // only the refusal is conditional, so the order must not be flipped.
            let exempt = !addresses_template(self);
            if let Err(exc) = claim(self.name(), session, target_rrid)
                && !exempt
            {
                restore_active(session, restore);
                return Err(exc);
            }
            // Exempt commands never touch the handed report; holding the guard
            // would fail concurrent calls with `template busy`.
            if exempt && !self.repoints_active() {
                session.release_active_guard();
            }
            let out = self.call(session, args).await;
            restore_active(session, restore);
            return out;
        }

        // `try_get_many` separates "never declared `-t`" (`Err`) from
        // "declared but unset" (`Ok(None)`) and "named hosts" (`Ok(Some)`).
        let hosts = args.try_get_many::<String>("hosts");
        let declares_hosts = hosts.is_ok();
        let named: Vec<String> = hosts
            .ok()
            .flatten()
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        let skippable_hostless =
            declares_hosts && named.is_empty() && self.skip_hostless_templates();
        // Only a template owning **none** of the named hosts is skippable; an
        // unknown host within one that owns some is a real error.
        let skippable_unowned = declares_hosts && !named.is_empty();

        let restore = session.templates.active_rrid().map(str::to_owned);
        // `is_hostless` locks the entry it inspects, so our own guard must go.
        session.release_active_guard();
        // Snapshotted once, before the loop, for [`Command::includes_detached_hosts`].
        let detached = if self.includes_detached_hosts() {
            session.detached_snapshot(&resolved)
        } else {
            std::collections::BTreeMap::new()
        };
        let mut failures: Vec<(String, CommandError)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        let mut cancelled = false;
        // Counted, never derived: deriving it from `resolved` would count every
        // never-reached template as done.
        let mut completed: Vec<&str> = Vec::new();
        // The flow's own cancel detail, preserved in the `Cancelled` payload.
        let mut body_stop: Option<(String, String)> = None;
        for rrid in &resolved {
            // Template boundary = cancellation checkpoint.
            if session.cancel_requested() {
                cancelled = true;
                break;
            }
            // `None` = held elsewhere, host set unreadable: only a *readable*
            // empty entry is skippable, so a contended one reaches `claim`.
            if skippable_hostless && session.is_hostless(rrid) == Some(true) {
                tracing::warn!(command = self.name(), rrid = %rrid, "skipped: no connected hosts");
                skipped.push(rrid.clone());
                continue;
            }
            // Same contention contract as above.
            if skippable_unowned && session.owns_none_of(rrid, &named) == Some(true) {
                let retained = detached
                    .get(rrid)
                    .is_some_and(|hosts| named.iter().any(|h| hosts.contains(h)));
                if !retained {
                    tracing::warn!(
                        command = self.name(),
                        rrid = %rrid,
                        hosts = ?named,
                        "skipped: none of the named hosts belong to this template"
                    );
                    skipped.push(rrid.clone());
                    continue;
                }
            }
            // An unclaimable template is that template's failure: bank it.
            if let Err(exc) = claim(self.name(), session, rrid) {
                failures.push((rrid.clone(), exc));
                continue;
            }
            session.display.template_banner(rrid);
            match self.call(session, args).await {
                Ok(()) => completed.push(rrid.as_str()),
                // A body stopped at its own checkpoint
                // (`commands::perform::map_flow_error`) *is* the cancel verdict,
                // not a template failure.
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
        // How far the fan-out got: the `Cancelled` payload, or the `FanOut`
        // aggregate's `stop` note when a real failure outranks the cancel.
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
            // A real per-template failure outranks the stop and falls through
            // to the `FanOut` aggregate below.
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
/// An empty rrid is the nothing-loaded session and dispatches on the sentinel.
/// Anything else that fails to claim the entry is refused and named: running
/// anyway would answer about nothing with exit 0, and the ERROR log alone never
/// reaches an MCP caller.
fn claim(command: &'static str, session: &mut Session, rrid: &str) -> CommandResult {
    match session.activate(rrid) {
        Activation::Active | Activation::Empty => Ok(()),
        Activation::Busy => {
            tracing::error!(command, rrid = %rrid, "activate lost the entry race");
            Err(CommandError::TemplateBusy(rrid.to_owned()))
        }
        // Resolution already checked the registry: an unload landed in between.
        Activation::NotLoaded => {
            tracing::error!(command, rrid = %rrid, "activate found no entry");
            Err(CommandError::TemplateNotLoaded(rrid.to_owned()))
        }
    }
}

/// Restores the active-template pointer (and its per-call handle) after
/// dispatch.
///
/// `restore` is `None` when nothing was active before or when
/// [`Command::repoints_active`] opted out; the guard is then refreshed onto
/// whatever the call left active.
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
/// An empty session resolves to a single empty-RRID entry (the null report), so
/// `run` takes the single-call path.
fn resolve_templates(
    command: &'static str,
    scope: Scope,
    session: &Session,
    args: &ArgMatches,
) -> Result<Vec<String>, CommandError> {
    if let Some(rrid) = arg_str(args, "template") {
        let rrid = canonical_template_name(&rrid);
        if session.templates.contains(&rrid) {
            return Ok(vec![rrid]);
        }
        return Err(CommandError::TemplateNotLoaded(rrid));
    }

    let active = || vec![session.templates.active_rrid().unwrap_or("").to_owned()];

    if scope == Scope::Single {
        return Ok(active());
    }

    // Bare `--all-templates` fans out regardless of scope.
    let all_templates = arg_all_templates(args);
    if all_templates == Some(true) {
        let all = session.templates.rrids();
        return Ok(if all.is_empty() { active() } else { all });
    }

    // `--all-templates=false`, or unset on `Scope::Explicit`: one template or refuse.
    if all_templates == Some(false) || scope == Scope::Explicit {
        return match session.resolve_single_template(None, session.is_repl) {
            SingleTemplate::One(rrid) => Ok(vec![rrid]),
            SingleTemplate::NothingLoaded => Ok(active()),
            SingleTemplate::Ambiguous(loaded) => {
                Err(CommandError::AmbiguousTemplate { command, loaded })
            }
            SingleTemplate::NotLoaded(rrid) => Err(CommandError::TemplateNotLoaded(rrid)),
        };
    }

    if scope == Scope::Fanout {
        let all = session.templates.rrids();
        return Ok(if all.is_empty() { active() } else { all });
    }

    // Headless with several loaded has no addressable active pointer: fan out.
    if !session.is_repl && session.templates.len() > 1 {
        return Ok(session.templates.rrids());
    }

    Ok(active())
}

/// Resolves the ordered *real* RRIDs a `command`/`argv` invocation would act on,
/// for callers that must know the targets *before* dispatch (the MCP
/// per-template lock gate).
///
/// `None` means the argv does not parse here, or resolves only to the null
/// report; the caller then serialises conservatively. Never errors: the real
/// error surfaces at dispatch.
#[must_use]
pub fn resolve_command_rrids(
    command: &dyn Command,
    session: &Session,
    argv: &[String],
) -> Option<Vec<String>> {
    match resolve_command_templates(command, session, argv) {
        ResolvedTemplates::Loaded(rrids) => Some(rrids),
        ResolvedTemplates::NullReport | ResolvedTemplates::Unresolved => None,
    }
}

/// What a `command`/`argv` invocation resolves to, from
/// [`resolve_command_templates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTemplates {
    /// These loaded templates, in dispatch order.
    Loaded(Vec<String>),
    /// Nothing loaded and no template named: the null report.
    NullReport,
    /// The argv does not parse here, names a template that is not loaded, or
    /// is ambiguous; the real error surfaces at dispatch.
    Unresolved,
}

/// [`resolve_command_rrids`] keeping the null report apart from a failed
/// resolution.
#[must_use]
pub fn resolve_command_templates(
    command: &dyn Command,
    session: &Session,
    argv: &[String],
) -> ResolvedTemplates {
    let parser = crate::engine::command_parser(command);
    let Ok(matches) = parser.try_get_matches_from(argv) else {
        return ResolvedTemplates::Unresolved;
    };
    match resolve_templates(command.name(), command.scope(), session, &matches) {
        Ok(resolved) if resolved == [""] => ResolvedTemplates::NullReport,
        Ok(resolved) => ResolvedTemplates::Loaded(resolved),
        Err(_) => ResolvedTemplates::Unresolved,
    }
}

/// Reads an optional string argument, tolerating a subcommand that never
/// declared it.
fn arg_str(args: &ArgMatches, id: &str) -> Option<String> {
    args.try_get_one::<String>(id).ok().flatten().cloned()
}

/// Reads the tri-state `--all-templates` flag: `Some(true)` (forced),
/// `Some(false)` (suppressed), or `None` (absent). Tolerates a subcommand that
/// never declared it.
fn arg_all_templates(args: &ArgMatches) -> Option<bool> {
    args.try_get_one::<bool>("all_templates")
        .ok()
        .flatten()
        .copied()
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
    /// ignores it, like `unload <rrid>` / `config` / `help`. Its parser
    /// declares neither template flag.
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

    /// An exempt [`Scope::Single`] probe (addresses no template) whose body
    /// blocks until released, so a test can observe the entry-guard state
    /// mid-call from another task.
    struct BlockingExempt {
        entered: tokio::sync::mpsc::Sender<()>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Command for BlockingExempt {
        fn name(&self) -> &'static str {
            "blocking_exempt_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn reads_resolved_report(&self) -> bool {
            false
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            self.entered.send(()).await.expect("test listens");
            self.release.notified().await;
            Ok(())
        }
    }

    /// An exempt command releases the active entry for its call: a concurrent
    /// dispatch on the same template must acquire it instead of failing
    /// `template busy` (#666).
    #[tokio::test]
    async fn exempt_single_releases_the_entry_for_the_call() {
        let (session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        let entry = session.templates.handle(RRID).expect("seeded");
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
        let release = Arc::new(tokio::sync::Notify::new());
        let probe = BlockingExempt {
            entered: entered_tx,
            release: Arc::clone(&release),
        };
        let parser = crate::engine::command_parser(&probe);
        let args = parser
            .try_get_matches_from(Vec::<String>::new())
            .expect("no argv");

        let handle = tokio::spawn(async move {
            let mut session = session;
            probe.run(&mut session, &args).await
        });
        entered_rx.recv().await.expect("probe entered its call");
        // The entry must be acquirable while the exempt call runs: the
        // entered signal already proves claim+release happened.
        let _held = entry
            .try_lock_owned()
            .expect("entry must not be held by an exempt call");
        release.notify_one();
        handle
            .await
            .expect("task ran")
            .expect("exempt call succeeds");
    }

    /// The release above applies exactly to the session-level commands: a new
    /// command joining this set must deliberately opt into guardless dispatch.
    #[test]
    fn exempt_commands_are_pinned() {
        let registry = crate::register_all();
        let mut exempt: Vec<&str> = registry
            .names()
            .filter(|n| {
                let command = registry.get(n).expect("registered");
                !addresses_template(command.as_ref())
            })
            .collect();
        exempt.sort_unstable();
        assert_eq!(
            exempt,
            vec![
                "config",
                "help",
                "list_refhosts",
                "list_templates",
                "load_template",
                "quit",
                "set_log_level",
                "switch",
                "teregen_login",
                "unload",
                "updates",
                "whoami",
            ]
        );
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

    /// The named-host analogue of the test above (#575): a contended template
    /// must fail rather than be mistaken for one that simply does not own the
    /// named `-t` host.
    #[tokio::test]
    async fn fanout_contended_template_with_named_host_fails_rather_than_being_skipped() {
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
        // `-t h1`: RRID owns h1, OTHER does not — without the contention guard
        // the locked RRID could misread as "does not own h1" and be skipped
        // instead of refused.
        let args = crate::commands::testkit::matches(&cmd, &["-t", "h1"]);

        let err = cmd
            .run(&mut session, &args)
            .await
            .expect_err("a contended template must fail, not be skipped as not owning the host");

        let CommandError::FanOut { failures, .. } = &err else {
            panic!("expected a per-template aggregate; got: {err:?}");
        };
        assert!(
            matches!(&failures[..], [(r, CommandError::TemplateBusy(_))] if r == RRID),
            "the contended template must be the one banked failure; got: {failures:?}"
        );
        assert!(
            seen.lock().expect("probe mutex").is_empty(),
            "OTHER does not own h1 and must be skipped, not run"
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

    /// The counterpart: a `Scope::Single` command that reads the report keeps
    /// `-T`, and naming the contended template with it still refuses, so the
    /// carve-out above is not a blanket exemption.
    #[tokio::test]
    async fn single_scope_reading_the_report_with_an_explicit_template_still_refuses() {
        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session.release_active_guard();
        let entry = session.templates.handle(RRID).expect("RRID is loaded");
        let _held = entry.try_lock_owned().expect("uncontended");

        let cmd = ReportReadingSingleScope(Arc::new(Mutex::new(Vec::new())));
        let parser = crate::engine::command_parser(&cmd);
        let args = parser
            .try_get_matches_from(["-T", RRID])
            .expect("a report-reading Single command keeps -T");

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

    /// A `Scope::Explicit` probe, for `resolve_command_rrids`'s own contract.
    struct ExplicitProbe;

    #[async_trait]
    impl Command for ExplicitProbe {
        fn name(&self) -> &'static str {
            "explicit_probe"
        }
        fn scope(&self) -> Scope {
            Scope::Explicit
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    fn resolve_named(session: &Session, template: &str) -> Result<Vec<String>, CommandError> {
        let parser = crate::engine::command_parser(&ExplicitProbe);
        let args = parser
            .try_get_matches_from(["-T", template])
            .expect("argv should parse");
        resolve_templates("explicit_probe", Scope::Explicit, session, &args)
    }

    #[test]
    fn short_template_flag_resolves_to_the_loaded_key() {
        let (session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        assert_eq!(resolve_named(&session, "S:M:1:1").unwrap(), vec![RRID]);
    }

    #[test]
    fn short_template_flag_not_loaded_reports_the_canonical_id() {
        let (session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        let err = resolve_named(&session, "S:M:9:9").unwrap_err();
        assert!(
            matches!(&err, CommandError::TemplateNotLoaded(r) if r == "SUSE:Maintenance:9:9"),
            "{err:?}"
        );
    }

    /// `resolve_command_rrids` swallows [`CommandError::AmbiguousTemplate`]
    /// via `.ok()?` exactly like any other resolution error: an ambiguous
    /// headless call yields `None`, so the MCP per-template lock gate falls
    /// back to serialising exclusively and the real refusal surfaces at
    /// dispatch instead of here.
    #[test]
    fn resolve_command_rrids_swallows_an_ambiguous_resolution() {
        const OTHER: &str = "SUSE:Maintenance:2:2";
        let (mut session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        session
            .templates
            .add(crate::commands::testkit::fake_report(OTHER, &["h2"], "ok"));
        session.is_repl = false;

        let rrids = resolve_command_rrids(&ExplicitProbe, &session, &[]);
        assert_eq!(
            rrids, None,
            "an ambiguous resolution must fall back to the exclusive gate, not panic or leak \
             the error"
        );
        assert_eq!(
            resolve_command_templates(&ExplicitProbe, &session, &[]),
            ResolvedTemplates::Unresolved
        );
    }

    #[test]
    fn resolve_command_templates_with_nothing_loaded_is_the_null_report() {
        let (session, _buf) = crate::commands::testkit::empty_session();
        assert_eq!(
            resolve_command_templates(&ExplicitProbe, &session, &[]),
            ResolvedTemplates::NullReport
        );
    }

    #[test]
    fn resolve_command_templates_naming_an_unloaded_template_is_unresolved() {
        let (session, _buf) = crate::commands::testkit::empty_session();
        let argv = ["-T".to_owned(), "SUSE:Maintenance:9:9".to_owned()];
        assert_eq!(
            resolve_command_templates(&ExplicitProbe, &session, &argv),
            ResolvedTemplates::Unresolved
        );
    }

    #[test]
    fn resolve_command_templates_with_a_template_loaded_names_it() {
        let (session, _buf) = session_with_hosts(RRID, &["h1"], "ok");
        assert_eq!(
            resolve_command_templates(&ExplicitProbe, &session, &[]),
            ResolvedTemplates::Loaded(vec![RRID.to_owned()])
        );
    }
}
