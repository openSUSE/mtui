//! Shared, explicitly-passed command state (`Session`).
//!
//! Commands receive `&mut Session` and mutate it through methods — no hidden
//! globals. It owns the [`Config`], the [`TemplateRegistry`], the
//! [`CommandPromptDisplay`] sink, and the `interactive` flag distinguishing the
//! REPL from headless callers such as `mtui-mcp`.
//! [`metadata`](Session::metadata) / [`targets`](Session::targets) delegate to
//! the active report, so command bodies keep a scalar surface as the registry
//! grows past one entry.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use mtui_config::Config;
use mtui_datasources::HttpError;
use mtui_datasources::http::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_datasources::refhost::{Attributes, Refhosts, RefhostsFactory, ResolveConfig, compare};
use mtui_hosts::{HostArbiter, HostError, HostsGroup, Owner, Prompter, Target};
use mtui_testreport::{NullReport, TestReport, UpdateKind, make_testreport};
use mtui_types::UpdateID;
use mtui_types::enums::{TargetState, Workflow};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::display::CommandPromptDisplay;
use crate::error::CommandError;
use crate::template_registry::{ReportEntry, TemplateRegistry};

/// Wall-clock budget for a host-close fan-out.
///
/// A wedged teardown (a dead peer with no RST whose close never returns) must
/// not block the REPL `quit`, template replacement/removal, or the MCP
/// idle-sweep's `close`: each bounds its fan-out with this budget and abandons
/// the straggler.
pub const HOST_CLOSE_TIMEOUT: Duration = Duration::from_secs(45);

/// The explicitly-passed state every command operates on.
pub struct Session {
    /// The application configuration.
    pub config: Config,
    /// Loaded templates and the active pointer.
    pub templates: TemplateRegistry,
    /// The per-call active-report handle: the [`OwnedMutexGuard`] of the entry
    /// this dispatch is acting on.
    ///
    /// [`metadata`](Self::metadata) / [`targets`](Self::targets) read through it
    /// when present and through [`null`](Self::null) otherwise, keeping a sync
    /// surface for command bodies. [`Command::run`](crate::Command::run)
    /// installs it per resolved template, dropping any prior guard *first* so
    /// one [`Session`] never self-deadlocks on an entry.
    active_guard: Option<OwnedMutexGuard<Box<dyn TestReport + Send + Sync>>>,
    /// The null-object fallback [`metadata`](Self::metadata) hands out when
    /// nothing is loaded (no [`active_guard`](Self::active_guard) installed).
    ///
    /// Never registered (its RRID is empty), so hosts attached while nothing is
    /// loaded are reachable only via
    /// [`take_teardown_units`](Self::take_teardown_units).
    null: Box<dyn TestReport + Send + Sync>,
    /// Formatted-output sink.
    pub display: CommandPromptDisplay,
    /// `true` for the interactive REPL, `false` for headless callers (MCP).
    ///
    /// Drives the fan-out default: headlessly there is no `switch` to pick an
    /// active template, so an unscoped command fans out over all of them.
    pub is_repl: bool,
    /// Set by `quit` to ask the interactive REPL loop to exit after the current
    /// dispatch returns.
    ///
    /// A flag plus `Ok(())` rather than routing process-exit through the command
    /// error channel; the REPL checks [`should_exit`](Self::should_exit) after
    /// each line and MCP ignores it.
    should_exit: bool,
    /// Optional sink for runtime log-level changes: a
    /// `tracing_subscriber::reload` handle under the REPL, `None` headlessly and
    /// in tests, where `set_log_level` logs the change but mutates nothing.
    log_level_sink: Option<LogLevelSink>,
    /// Optional sink for best-effort desktop notifications, backed by
    /// `mtui-cli`'s `notification::notify_user` under the REPL and `None`
    /// elsewhere — keeping notifications a REPL-only courtesy and `mtui-core`
    /// free of the CLI notification backend.
    notify_sink: Option<NotifySink>,
    /// The session-level serialised interactive [`Prompter`], or `None`
    /// headlessly, where a command timeout aborts immediately.
    ///
    /// The composition root (`mtui-cli`'s `main.rs`) installs a
    /// [`Prompter::stdin`]-backed one via [`set_prompter`](Self::set_prompter).
    /// It reaches two places: the command-timeout prompt on each freshly-built
    /// [`Target`] in [`connect_and_add_hosts`](Self::connect_and_add_hosts), and
    /// the active report's [`HostsGroup`] via [`HostsGroup::set_prompter`].
    prompter: Option<Prompter>,
    /// Test-only count of how many times [`http_client`](Self::http_client)
    /// actually *built* a client, as opposed to handing back a cached clone.
    #[cfg(test)]
    http_builds: std::sync::atomic::AtomicUsize,
    /// Per-slot candidate shuffle, so pool selection spreads load across
    /// interchangeable refhosts instead of always taking the first in
    /// `refhosts.yml` order. Tests override it with the identity.
    shuffle: ShuffleFn,
    /// The cancellation seam: cooperative cancel signal for the dispatch this
    /// session is driving (MCP `job_cancel`).
    ///
    /// Freshly minted for a REPL/canonical session;
    /// [`fork_for_call`](Self::fork_for_call) *clones* it so a fork shares the
    /// parent's state, and `mtui-mcp` installs a per-job token via
    /// [`set_cancel_token`](Self::set_cancel_token).
    /// [`Command::run`](crate::Command::run) checks it before dispatch and
    /// between fan-out templates; a long-running body may also observe it. Purely
    /// cooperative: a body that never checks is hard-aborted by the MCP job
    /// layer after its grace period.
    cancel: CancellationToken,
    /// Lazily-built, session-scoped outbound [`HttpClient`], cached with the
    /// [`VerifyPolicy`] it was built under.
    ///
    /// `reqwest` fixes TLS and owns its connection pool at build time, so a
    /// per-command client means a cold pool per command; this one is built once
    /// and cloned, rebuilding only when the posture changes. Interior mutability
    /// lets the `&Session` call sites (`export::build_http`) populate it lazily;
    /// the lock is uncontended (one dispatch at a time).
    http_client: Mutex<Option<(VerifyPolicy, HttpClient)>>,
    /// Lazily-built, session-scoped openQA transport: a redirect-less,
    /// no-reqwest-retry `reqwest::Client` for
    /// `ruoqa::ClientBuilder::http_client`, cached like
    /// [`http_client`](Self::http_client) so back-to-back openQA connectors
    /// (`reload_openqa`'s primary + baremetal instances) share one pool.
    openqa_transport: Mutex<Option<(VerifyPolicy, reqwest::Client)>>,
}

/// A candidate-order shuffle seam: mutates the slot's candidate list in place
/// before the arbiter picks one.
pub type ShuffleFn = fn(&mut [String]);

/// The default [`ShuffleFn`]: a real random shuffle.
fn random_shuffle(candidates: &mut [String]) {
    use rand::seq::SliceRandom;
    candidates.shuffle(&mut rand::rng());
}

/// Render a refhosts [`Slot`](mtui_datasources::refhost::Slot) as the stable
/// `product|version|arch|addon,addon` key
/// [`TestReportBase::slot_candidates`](mtui_testreport::TestReportBase::slot_candidates)
/// groups a slot's backup candidates under. The tuple already sorts its addons,
/// so the encoding is a deterministic 1:1.
fn slot_key(slot: &mtui_datasources::refhost::Slot) -> String {
    let (product, version, arch, addons) = slot;
    format!("{product}|{version}|{arch}|{}", addons.join(","))
}

/// The log levels `set_log_level` accepts (`info`/`warning`/`error`/
/// `debug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Warnings and above.
    Warning,
    /// Informational and above (the default).
    Info,
    /// Everything, incl. debug tracing.
    Debug,
}

impl LogLevel {
    /// Parses the level name, or `None` if unrecognised.
    #[must_use]
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "error" => Some(Self::Error),
            "warning" => Some(Self::Warning),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    /// The corresponding [`tracing::Level`].
    #[must_use]
    pub fn as_tracing(self) -> tracing::Level {
        match self {
            Self::Error => tracing::Level::ERROR,
            Self::Warning => tracing::Level::WARN,
            Self::Info => tracing::Level::INFO,
            Self::Debug => tracing::Level::DEBUG,
        }
    }
}

/// A callback the REPL installs to apply a runtime log-level change.
pub type LogLevelSink = Box<dyn FnMut(LogLevel) + Send>;

/// A callback the REPL installs to surface a desktop notification, called with
/// the message and `true` for error-class toasts (a `stock_dialog-error` icon).
pub type NotifySink = Box<dyn FnMut(&str, bool) + Send>;

impl Session {
    /// Builds a session for `config`, defaulting the display to stdout.
    ///
    /// `interactive` is `true` for the REPL, `false` for MCP.
    #[must_use]
    pub fn new(config: Config, is_repl: bool) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        let null: Box<dyn TestReport + Send + Sync> = Box::new(NullReport::new(config.clone()));
        Self {
            config,
            templates,
            active_guard: None,
            null,
            display: CommandPromptDisplay::stdout(),
            is_repl,
            should_exit: false,
            log_level_sink: None,
            notify_sink: None,
            prompter: None,
            shuffle: random_shuffle,
            cancel: CancellationToken::new(),
            http_client: Mutex::new(None),
            openqa_transport: Mutex::new(None),
            #[cfg(test)]
            http_builds: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Builds a session with an explicit display sink (test/embedding seam).
    #[must_use]
    pub fn with_display(config: Config, is_repl: bool, display: CommandPromptDisplay) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        let null: Box<dyn TestReport + Send + Sync> = Box::new(NullReport::new(config.clone()));
        Self {
            config,
            templates,
            active_guard: None,
            null,
            display,
            is_repl,
            should_exit: false,
            log_level_sink: None,
            notify_sink: None,
            prompter: None,
            shuffle: random_shuffle,
            cancel: CancellationToken::new(),
            http_client: Mutex::new(None),
            openqa_transport: Mutex::new(None),
            #[cfg(test)]
            http_builds: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Builds a cheap per-call [`Session`] that **shares** this session's loaded
    /// reports and carries its own display sink.
    ///
    /// The headless MCP concurrency seam: a single-RRID tool call needs a
    /// `&mut Session`, but holding the canonical one behind a mutex across
    /// dispatch serialises *all* calls. A fork's `TemplateRegistry::snapshot`
    /// shares the per-entry report locks, so a command on RRID `X` locks only
    /// `X`'s entry and its mutations stay visible to the canonical session.
    ///
    /// It copies `is_repl`/`shuffle` — only mutated by `Scope::Single` commands,
    /// which run against the *canonical* session under the MCP exclusive gate —
    /// and starts with an empty `http_client` cache and no prompter/sinks. Only
    /// a **single-real-template**, non-mutating command is therefore sound to
    /// dispatch through a fork.
    #[must_use]
    pub fn fork_for_call(&self, display: CommandPromptDisplay) -> Self {
        // A host added on a fork while nothing is loaded would land in this
        // private `null` and be discarded with the fork, reachable by no
        // teardown. Two load-bearing mechanisms keep that unreachable (#478):
        // nothing loaded never forks (`resolve_command_rrids` returns `None`,
        // which `command_lock` maps to the exclusive arm on the canonical
        // session), and that exclusive path releases the canonical active guard
        // on the way out so a later fork's `activate` can re-lock the entry
        // rather than fall through to its own null. The second half is enforced
        // in `mtui-mcp`, a higher crate.
        let null: Box<dyn TestReport + Send + Sync> =
            Box::new(NullReport::new(self.config.clone()));
        Self {
            config: self.config.clone(),
            templates: self.templates.snapshot(),
            active_guard: None,
            null,
            display,
            is_repl: self.is_repl,
            should_exit: false,
            log_level_sink: None,
            notify_sink: None,
            prompter: None,
            shuffle: self.shuffle,
            // Cancelling either side — the canonical session or the per-job
            // token MCP installs on this fork — must be observable on both.
            cancel: self.cancel.clone(),
            http_client: Mutex::new(None),
            openqa_transport: Mutex::new(None),
            #[cfg(test)]
            http_builds: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// `true` once cancellation has been requested for this dispatch.
    ///
    /// The cheap poll form of the seam, for a body checking at its own
    /// host/step boundaries. [`check_cancelled`](Self::check_cancelled) is the
    /// `?`-friendly form, [`cancel_token`](Self::cancel_token) the awaitable one.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Bail with [`CommandError::Cancelled`] if cancellation has been requested.
    ///
    /// The `?`-friendly checkpoint the [`Command::run`](crate::Command::run)
    /// driver calls before dispatch and between fan-out templates.
    ///
    /// # Errors
    ///
    /// [`CommandError::Cancelled`] once the token has been cancelled.
    pub fn check_cancelled(&self) -> Result<(), CommandError> {
        if self.cancel.is_cancelled() {
            return Err(CommandError::Cancelled(String::new()));
        }
        Ok(())
    }

    /// A clone of the session's cancellation token (clones share state), for
    /// `select!`-style bodies wanting `token.cancelled().await` as a branch.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Install `token` as this session's cancellation token.
    ///
    /// The MCP job layer wires one freshly minted token per background job in,
    /// so `job_cancel` cancels exactly that job. Installing *replaces* rather
    /// than chains, so a caller whose session outlives the call (the exclusive
    /// dispatch path) must restore the prior token afterwards.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel = token;
    }

    /// The session-scoped outbound [`HttpClient`], built lazily and reused.
    ///
    /// The effective [`VerifyPolicy`] comes from `config.ssl_verify`, and the
    /// cache is keyed on it, so a mid-session `config set ssl_verify` rebuilds
    /// rather than leaving TLS behaviour stale. Otherwise later calls return a
    /// cheap `Arc`-backed clone sharing one `reqwest` connection pool.
    ///
    /// # Errors
    ///
    /// Propagates [`HttpError`] when the client cannot be built (e.g. an
    /// unreadable CA bundle).
    pub(crate) fn http_client(&self) -> Result<HttpClient, HttpError> {
        let policy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&self.config.ssl_verify)),
        );
        let mut cache = self
            .http_client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_policy, client)) = cache.as_ref()
            && *cached_policy == policy
        {
            return Ok(client.clone());
        }
        let client = HttpClient::new(policy.clone())?;
        #[cfg(test)]
        self.http_builds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *cache = Some((policy, client.clone()));
        Ok(client)
    }

    /// Test-only count of clients actually built by
    /// [`http_client`](Self::http_client).
    #[cfg(test)]
    fn http_builds(&self) -> usize {
        self.http_builds.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The session-scoped openQA transport, built lazily and reused.
    ///
    /// Caches by [`VerifyPolicy`] like [`http_client`](Self::http_client), but
    /// hands out a bare `reqwest::Client` rather than the [`HttpClient`]
    /// wrapper: `ruoqa` re-signs and redirects every request itself, so the
    /// transport it owns must not follow redirects or retry at the reqwest level
    /// (see [`HttpClient::openqa_transport`]).
    ///
    /// # Errors
    ///
    /// Propagates [`HttpError`] when the transport cannot be built (e.g. an
    /// unreadable CA bundle).
    pub(crate) fn openqa_transport(&self) -> Result<reqwest::Client, HttpError> {
        let policy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&self.config.ssl_verify)),
        );
        let mut cache = self
            .openqa_transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_policy, transport)) = cache.as_ref()
            && *cached_policy == policy
        {
            return Ok(transport.clone());
        }
        let transport = HttpClient::openqa_transport(policy.clone())?;
        *cache = Some((policy, transport.clone()));
        Ok(transport)
    }

    /// Makes `rrid` the active template *and* installs its per-call active
    /// handle (`active_guard`).
    ///
    /// The unified activation seam. Any prior guard is dropped first, so
    /// re-activating the same entry never self-deadlocks — and `try_lock_owned`
    /// then suffices, the outer session mutex having serialised dispatch.
    /// Returns `false` (guard cleared) if `rrid` is not loaded; an empty `rrid`
    /// clears pointer and guard so `metadata()` falls back to the null object.
    pub fn activate(&mut self, rrid: &str) -> bool {
        self.active_guard = None;
        if rrid.is_empty() {
            self.templates.set_active_none();
            return false;
        }
        if !self.templates.set_active(rrid) {
            return false;
        }
        self.active_guard = self
            .templates
            .active_handle()
            .and_then(|h| h.try_lock_owned().ok());
        // Pushing the token down here — the one place every dispatch passes
        // through to reach a report — gives every command the host-boundary seam
        // without opting in, and refreshes it per activation so a group never
        // carries a stale cancelled token from an earlier job.
        let cancel = self.cancel.clone();
        if let Some(guard) = self.active_guard.as_mut() {
            guard.base_mut().targets.set_cancel_token(cancel);
        }
        self.active_guard.is_some()
    }

    /// Drops the per-call active handle *without* changing the active pointer.
    ///
    /// Teardown/probe paths that lock entries directly (`quit`/`unload`/`load`
    /// replace, MCP `close`, the fan-out `is_hostless` probe) would self-deadlock
    /// on an entry this session's guard still holds. Unlike `activate("")` the
    /// registry's active RRID is left intact, so a survivor can still be promoted.
    pub fn release_active_guard(&mut self) {
        self.active_guard = None;
    }

    /// Every host group this session owns, as independently-lockable teardown
    /// units: one per registry entry, plus the sentinel's group when it holds
    /// hosts.
    ///
    /// The single enumeration teardown walks (`quit`, `McpSession::close`, the
    /// MCP idle sweep). The registry alone is *not* the set of connected hosts:
    /// `add_host` with nothing loaded writes into the private sentinel, whose
    /// empty RRID no `rrids()` walk can return. A non-empty sentinel group is
    /// **moved out** — swapped for a fresh [`NullReport`] — so it can outlive
    /// the caller's borrow (`McpSession::close` cannot hold the session mutex
    /// across its teardown awaits) and cannot be handed out twice. The per-call
    /// active handle is released first, since teardown locks *every* entry; the
    /// registry's active pointer is left intact.
    ///
    /// # Warning
    ///
    /// Dropping the returned `Vec` instead of tearing down every unit strands
    /// the sentinel's hosts with their remote `/var/lock/mtui.lock` held — this
    /// call is the only `Arc` created for them.
    #[must_use]
    pub fn take_teardown_units(&mut self) -> Vec<ReportEntry> {
        self.release_active_guard();
        // The sentinel goes first: under a shared deadline (see
        // `McpSession::close_with_timeout`) it is the only unrecoverable unit,
        // and typically the smallest group, so it is cheap insurance.
        let mut units: Vec<ReportEntry> = Vec::new();
        if !self.null.base().targets.is_empty() {
            let stranded = std::mem::replace(
                &mut self.null,
                Box::new(NullReport::new(self.config.clone())),
            );
            units.push(std::sync::Arc::new(tokio::sync::Mutex::new(stranded)));
        }
        units.extend(
            self.templates
                .rrids()
                .into_iter()
                .filter_map(|rrid| self.templates.handle(&rrid)),
        );
        units
    }

    /// Re-installs the active handle for the registry's current active pointer,
    /// for after a registry mutation repoints `active` without going through
    /// [`activate`](Self::activate).
    pub(crate) fn refresh_active_guard(&mut self) {
        self.active_guard = None;
        self.active_guard = self
            .templates
            .active_handle()
            .and_then(|h| h.try_lock_owned().ok());
    }

    /// Whether `rrid` is the active template *and* this session currently holds
    /// its per-call handle.
    ///
    /// Lets guard-unaware callers (the hand-written MCP testreport tools) choose
    /// between [`metadata`](Self::metadata) and locking the entry handle.
    #[must_use]
    pub fn active_report_is_guarded(&self, rrid: &str) -> bool {
        self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid)
    }

    /// Whether the report loaded under `rrid` has no connected hosts.
    ///
    /// The guard-aware counterpart of
    /// [`TemplateRegistry::is_hostless`](crate::TemplateRegistry::is_hostless):
    /// the active entry is already locked by this session's
    /// [`active_guard`](Self::active_guard), so it must be read through the
    /// guard rather than a `try_lock` that would fail.
    #[must_use]
    pub(crate) fn is_hostless(&self, rrid: &str) -> bool {
        if self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid) {
            self.metadata().base().targets.is_empty()
        } else {
            self.templates.is_hostless(rrid)
        }
    }

    /// The connected-host count and workflow label for `rrid`, or `None` if
    /// absent (for `list_templates`).
    ///
    /// Guard-aware, like [`is_hostless`](Self::is_hostless).
    #[must_use]
    pub(crate) fn template_row(&self, rrid: &str) -> Option<(usize, &'static str)> {
        if self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid) {
            let base = self.metadata().base();
            Some((base.targets.len(), base.workflow.as_str()))
        } else {
            self.templates.template_row(rrid)
        }
    }

    /// The active report. Never `None`: falls back to the null object when
    /// nothing is loaded.
    #[must_use]
    pub fn metadata(&self) -> &(dyn TestReport + Send + Sync) {
        match &self.active_guard {
            Some(g) => &***g,
            None => &*self.null,
        }
    }

    /// Mutably borrows the active report.
    ///
    /// The mutable counterpart of [`metadata`](Self::metadata), through which
    /// `reload_openqa` / `set_workflow` populate the report's openQA holder
    /// ([`TestReport::openqa_mut`]). Never `None`.
    pub(crate) fn metadata_mut(&mut self) -> &mut (dyn TestReport + Send + Sync) {
        match &mut self.active_guard {
            Some(g) => (**g).as_mut(),
            None => self.null.as_mut(),
        }
    }

    /// Sets the active report's [`Workflow`] mode.
    ///
    /// The one mutable window onto it — `add_host` uses it to move an automatic
    /// session to manual. Refreshing the REPL prompt string is a separate REPL
    /// concern.
    pub(crate) fn set_workflow(&mut self, workflow: Workflow) {
        self.metadata_mut().base_mut().workflow = workflow;
    }

    /// The active report's connected targets.
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.metadata().base().targets
    }

    /// Mutably borrows the active report's connected targets.
    ///
    /// The mutable counterpart of [`targets`](Self::targets), for bodies that
    /// fan out across hosts (`run`, `reboot`, `set_repo`).
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.metadata_mut().base_mut().targets
    }

    /// Moves the active report's targets out, leaving an empty group in place.
    ///
    /// The report's `perform_*` methods take `&self` **and** `&mut HostsGroup`,
    /// which one `&mut Box<dyn TestReport>` cannot hand out at once because the
    /// targets live inside the report. Taking the group out by value breaks the
    /// tie; [`restore_targets`](Self::restore_targets) puts it back.
    #[must_use]
    fn take_targets(&mut self) -> HostsGroup {
        let is_repl = self.is_repl;
        std::mem::replace(
            &mut self.metadata_mut().base_mut().targets,
            HostsGroup::new(Vec::new(), is_repl),
        )
    }

    /// Restores the active report's targets, undoing [`take_targets`](Self::take_targets).
    fn restore_targets(&mut self, targets: HostsGroup) {
        self.metadata_mut().base_mut().targets = targets;
    }

    /// Takes the active report's targets and splits them into the `-t` selection
    /// and the unselected remainder.
    ///
    /// A `-t` subset operation must run over only the selected hosts, yet the
    /// unselected ones must survive in the live report — and a `Target` owns its
    /// connection, so a child group cannot borrow references into the parent's.
    /// Both halves come back, and
    /// [`restore_split_targets`](Self::restore_split_targets) merges the
    /// remainder in afterwards.
    ///
    /// `hosts` is the parsed `-t` value: `None` (also how callers pass `-t all`)
    /// selects every enabled host with an empty remainder; `Some` names exactly
    /// those and keeps the rest. `enabled` gates selection as for
    /// [`HostsGroup::select_split`](mtui_hosts::HostsGroup::select_split): a
    /// named-but-disabled host lands in the remainder, never dropped.
    ///
    /// # Errors
    ///
    /// [`mtui_hosts::HostError::NotConnected`] when a named `-t` host is not a
    /// member of the active report's group. The failed split consumes the taken
    /// group, leaving the report's empty; callers surface the error immediately,
    /// so no host is observable in that window.
    pub(crate) fn split_targets(
        &mut self,
        hosts: Option<&[String]>,
        enabled: bool,
    ) -> mtui_hosts::Result<(HostsGroup, HostsGroup)> {
        self.take_targets().select_split(hosts, enabled)
    }

    /// Merges the untouched `remainder` back into the operated `selected` group
    /// and restores it as the active report's targets.
    ///
    /// The counterpart to [`split_targets`](Self::split_targets): recombining
    /// preserves the hosts a `-t` subset operation did not touch.
    pub(crate) fn restore_split_targets(
        &mut self,
        mut selected: HostsGroup,
        remainder: HostsGroup,
    ) {
        selected.merge(remainder);
        self.restore_targets(selected);
    }

    /// Loads a template into the registry and, when requested, connects its
    /// reference hosts.
    ///
    /// 1. [`make_testreport`] checks out and reads the report (or returns a null
    ///    report on failure, which [`TemplateRegistry::add`] silently ignores).
    /// 2. The report is registered and, with a real RRID, made active.
    ///    Re-loading an already-loaded RRID replaces its stored report;
    ///    siblings are untouched.
    /// 3. If it asked for autoconnect
    ///    ([`TestReportBase::autoconnect_pending`](mtui_testreport::TestReportBase::autoconnect_pending)),
    ///    its reference hosts are connected — driven **here**, the composition
    ///    root, so `mtui-testreport` never depends on
    ///    `mtui-hosts`/`mtui-datasources`. Best-effort: an unreachable host is
    ///    logged and skipped, never aborting the load.
    ///
    /// Returns the loaded report's RRID, empty when the load failed and the null
    /// report was substituted. A thin wrapper over `load_update_reported` that
    /// discards the failure reason.
    pub async fn load_update(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
    ) -> String {
        self.load_update_reported(update, autoconnect, kind).await.0
    }

    /// [`load_update`](Self::load_update) that also returns *why* a load failed.
    ///
    /// Returns `(rrid, load_error)`: on failure an empty RRID plus the
    /// diagnostic [`make_testreport`] stashed on the substituted null report
    /// (svn checkout / gitea / hash / read failure), so `load_template` can
    /// surface the real cause instead of a bare "could not load".
    pub(crate) async fn load_update_reported(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
    ) -> (String, Option<String>) {
        let report = make_testreport(
            update,
            self.config.clone(),
            kind,
            autoconnect,
            self.is_repl,
            self.prompter.as_ref(),
        )
        .await;
        let rrid = report.id();
        let pending = report.base().autoconnect_pending;
        // Capture the reason before `add_or_replace` moves (and, for the null
        // sentinel, drops) the report.
        let load_error = if rrid.is_empty() {
            report.base().load_error.clone()
        } else {
            None
        };

        // `add_or_replace` tears the old report down by locking its entry, which
        // would self-deadlock against a guard this session still holds (e.g.
        // `regenerate` reloading the active template). Re-installed below.
        self.active_guard = None;

        let removed = self.templates.add_or_replace(report).await;
        for (host, err) in &removed.failed {
            tracing::warn!("failed to disconnect from {host} while reloading: {err}");
        }
        for host in &removed.stragglers {
            tracing::warn!("still disconnecting from {host} while reloading");
        }
        if !rrid.is_empty() {
            self.templates.set_active(&rrid);
        }
        // Restores the guard released above, onto the freshly-loaded template or
        // (on a failed load, where the pointer never moved) the prior active, so
        // the autoconnect below and the caller both read through `metadata()`.
        self.refresh_active_guard();

        // A Product Increment under `[lock] pi_autolock` is locked for the life
        // of the loaded report, not bracketed around the review workflow, and
        // `lock_connected_target` locks each host as it arrives — so seeding the
        // comment before any host connects is all the acquire side needs.
        if self.config.lock_pi_autolock
            && let Some(loaded_rrid) = self.metadata().rrid()
            && loaded_rrid.kind == mtui_types::RequestKind::Pi
        {
            let comment = format!("testing of {loaded_rrid}");
            self.metadata_mut().base_mut().lock_comment = comment;
        }

        if pending && !rrid.is_empty() {
            self.autoconnect_active(&rrid).await;
        }
        (rrid, load_error)
    }

    /// Connects the active report's reference hosts (the deferred half of
    /// [`load_update`](Self::load_update)).
    ///
    /// Resolves the wanted hosts — the template's parsed `reference host:` names
    /// plus one host per matching slot from each testplatform — then connects a
    /// [`Target`] for each, stamping the report's RRID as the pool-claim
    /// ownership identity.
    async fn autoconnect_active(&mut self, rrid: &str) {
        // Snapshot synchronously: `Session` is not `Sync`, so a borrow held
        // across the resolver await would make this future non-`Send`, which the
        // `Command::call` trait requires.
        let config = self.config.clone();
        let shuffle = self.shuffle;
        let (mut ref_hosts, already, testplatforms, arbiter, owner) = {
            let base = self.metadata().base();
            (
                base.hostnames.iter().cloned().collect::<Vec<_>>(),
                base.targets.names(),
                base.testplatforms.clone(),
                base.arbiter,
                base.owner.clone(),
            )
        };
        // Deterministic ref-host order (`hostnames` is a HashSet).
        ref_hosts.sort();
        ref_hosts.dedup();

        // Testplatform hosts go through pool selection (one host per requested
        // slot) when the arbiter + owner are wired — the default.
        let wanted = self
            .resolve_and_record_pool(&config, ref_hosts, testplatforms, arbiter, owner, shuffle)
            .await
            .into_iter()
            .filter(|h| !already.contains(h))
            .collect();

        self.connect_and_add_hosts(wanted, rrid).await;
    }

    /// Builds a live [`Target`] for each host in `hosts`, connects it, and adds
    /// the ones that connect to the active report's group; connect failures are
    /// logged and skipped so one bad host never aborts the batch.
    ///
    /// The shared connect loop behind
    /// [`autoconnect_active`](Self::autoconnect_active) and `add_host`. Each
    /// target is stamped with `rrid` as its pool-claim ownership identity;
    /// [`Target::connect`] short-circuits for a caller that pre-builds connected
    /// targets (tests over a mock connection).
    ///
    /// A freshly connected host is autolocked with the active report's
    /// `lock_comment` when a PI assignment is in progress; one already locked by
    /// another owner is left as-is ([`HostError::TargetLocked`] suppressed), and
    /// a failed autolock never drops an otherwise-good host. It is also checked
    /// for product drift against its `refhosts.yml` row
    /// ([`verify_target_products`](Self::verify_target_products)) — surfaced,
    /// recorded in `product_warnings` and WARN-logged, but never dropping the
    /// host, and skipped entirely if the inventory is unavailable.
    async fn connect_and_add_hosts(&mut self, hosts: Vec<String>, rrid: &str) {
        let config = self.config.clone();
        // Everything below is snapshotted before the connect loop: a `base()` or
        // `&Prompter` borrow held across the connect `.await` would make this
        // future non-`Send`, which the `Command::call` bound requires.
        // Empty when no PI assignment is active.
        let lock_comment = self.metadata().base().lock_comment.clone();
        // `product -> { name -> required-version }`, seeding each host's tracked
        // packages right after connect(); empty makes the seeding a no-op.
        let package_meta = self.metadata().base().packages.clone();
        // `None` (headless / `mtui-mcp`) leaves the timeout an immediate abort.
        let timeout_prompt = self.prompter.as_ref().map(Prompter::as_timeout_prompt);
        let prompter = self.prompter.clone();
        // One inventory for the whole batch, built before the `targets_mut()`
        // borrow so this await does not straddle it. `None` disables the drift
        // check for every host — best-effort, never fatal.
        let store = Self::build_refhosts_store(&config).await;
        // Each host's connect + autolock + package-seed + drift-verify is an
        // independent future driven with the rest of the batch, so attaching N
        // hosts costs one slow handshake rather than the sum.
        // Whether a host takes the remote pool lock or the normal autolock;
        // empty on the legacy `add_host --target` path.
        let pool_claims = self.metadata().base().pool_claims.clone();
        let store_ref = store.as_ref();
        let package_meta = &package_meta;
        let timeout_prompt = &timeout_prompt;
        let lock_comment = &lock_comment;
        let config_ref = &config;
        let pool_claims_ref = &pool_claims;
        let connect_futs = hosts.iter().map(|host| {
            Self::connect_one(
                config_ref,
                host.clone(),
                rrid,
                timeout_prompt,
                lock_comment,
                package_meta,
                store_ref,
                pool_claims_ref.contains(host),
            )
        });
        // Bound the fan-out to `[connection] max_parallel` so a large fleet caps
        // peak concurrent SSH handshakes/sockets/tasks. The futures borrow
        // `&config`/`&store` (a spawn-free in-place fan-out) and so do not fit
        // `buffer_unordered`'s stream bounds — hence chunk-and-`join_all`.
        // Completion order is irrelevant: results fold into a sorted `BTreeMap`.
        let bound = (config.max_parallel as usize).max(1);
        let connect_futs: Vec<_> = connect_futs.collect();
        let mut connected = Vec::with_capacity(connect_futs.len());
        let mut iter = connect_futs.into_iter().peekable();
        while iter.peek().is_some() {
            let batch: Vec<_> = iter.by_ref().take(bound).collect();
            connected.extend(futures::future::join_all(batch).await);
        }

        let mut drift: Vec<(String, Option<Vec<String>>)> = Vec::new();
        // Which slots the pool-backup step below still needs a live host for.
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let targets = self.targets_mut();
        // A group built by a later `load_update` would otherwise start without a
        // prompter, so its command-timeout prompt would never fire.
        if let Some(prompter) = prompter {
            targets.set_prompter(prompter);
        }
        for (target, drift_entry) in connected.into_iter().flatten() {
            live.insert(target.hostname().to_owned());
            targets.add(target);
            drift.push(drift_entry);
        }

        let backup_drift = self
            .connect_pool_backups(&config, rrid, &hosts, &live, timeout_prompt.clone())
            .await;
        drift.extend(backup_drift);

        // Only now is the `targets_mut()` borrow released.
        self.apply_product_warnings(drift);
    }

    /// Takes the pool claim and/or operation lock on an already-connected
    /// `target`. Returns `false` when a pool claim was required but lost the
    /// remote race, meaning the caller should drop the host.
    ///
    /// The two branches are sequential, not mutually exclusive: a non-empty
    /// `lock_comment` (a loaded Product Increment) takes the operation lock
    /// regardless of `is_pool_claim`, so a pool-selected host is PI-locked too.
    /// Split out of [`connect_one`](Self::connect_one) — where it was an
    /// `if/else` that left a pool host never PI-locked — so it is exercisable
    /// against a [`MockConnection`](mtui_hosts::MockConnection) without a live
    /// SSH connect.
    async fn lock_connected_target(
        target: &mut Target,
        host: &str,
        rrid: &str,
        lock_comment: &str,
        is_pool_claim: bool,
    ) -> bool {
        if is_pool_claim {
            // Losing the remote race means another process holds this host —
            // drop it so a sibling in the slot can be tried (the in-process
            // claim is released by `connect_pool_backups`).
            let comment = format!("mtui pool {rrid} [{rrid}]");
            match target.pool_claim(&comment).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(host = %host, "claimed in-process but busy remotely; skipping");
                    return false;
                }
                Err(e) => {
                    warn!(host = %host, error = %e, "pool claim failed remotely; skipping");
                    return false;
                }
            }
        }
        Self::autolock_target(target, lock_comment).await;
        true
    }

    /// Connects a single host, autolocks + package-seeds + drift-verifies it,
    /// and returns the live [`Target`] plus its drift entry, or `None` on
    /// failure.
    ///
    /// One path shared by the concurrent initial batch and the sequential
    /// backup-refhost fallback
    /// ([`connect_pool_backups`](Self::connect_pool_backups)). All inputs are
    /// plain data, so the returned future stays `Send`.
    #[allow(clippy::too_many_arguments)]
    async fn connect_one(
        config: &Config,
        host: String,
        rrid: &str,
        timeout_prompt: &Option<mtui_hosts::TimeoutPrompt>,
        lock_comment: &str,
        package_meta: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
        store: Option<&Refhosts>,
        is_pool_claim: bool,
    ) -> Option<(Target, (String, Option<Vec<String>>))> {
        let mut target = Target::new(config, host.clone(), TargetState::Enabled);
        target.set_rrid(rrid.to_owned());
        // Before connecting, so `Target::connect` applies it to the transport.
        if let Some(tp) = timeout_prompt.as_ref() {
            target.set_timeout_prompt(tp.clone());
        }
        match target.connect().await {
            Ok(()) => {
                if !Self::lock_connected_target(
                    &mut target,
                    &host,
                    rrid,
                    lock_comment,
                    is_pool_claim,
                )
                .await
                {
                    return None;
                }
                // Seed the tracked packages with their metadata `required`
                // versions and query the current ones, so `list_packages` /
                // `package_check` / `downgrade` see a populated list.
                // `connect()` already parsed the system, so `get_base().version`
                // is authoritative here.
                let base_version = target.system().get_base().version.clone();
                let seeded =
                    mtui_testreport::testreport::packages_for_map(package_meta, &base_version);
                if seeded.is_empty() {
                    // #396: tracking nothing means before/after version checks
                    // cannot run and export has nothing to verify — say so
                    // rather than skipping silently.
                    warn!(
                        host = %host, base_version = %base_version,
                        "report metadata names no packages for this host's base product; \
                         package list not seeded — version checks cannot run"
                    );
                } else {
                    target.set_packages(seeded);
                    target.query_versions().await;
                }
                let drift = Self::verify_target_products(store, &target);
                Some((target, (host, drift)))
            }
            Err(e) => {
                warn!(host = %host, "connect failed, skipping: {e}");
                None
            }
        }
    }

    /// Retries failed pool slots against their remaining free candidates
    /// (RFC §5.7 backup-refhost).
    ///
    /// For each slot in `slot_candidates` whose chosen host is not among the
    /// just-connected `live` hosts: drop the dead claim(s), then sequentially
    /// `acquire_any` the next free sibling and connect it until one succeeds or
    /// the candidates are exhausted. Any host that connects joins the active
    /// group and returns its drift entry. A no-op when pool selection is
    /// inactive (`arbiter`/`owner` unset) or no slots are recorded; best-effort,
    /// so a connect failure releases the in-process claim and moves on.
    ///
    /// The whole phase shares one wall-clock budget (`4 * connect_timeout`), not
    /// a per-slot one: a per-slot budget still multiplies by slot count, so a
    /// report with many dead slots could wedge a caller for
    /// `slots * siblings * connect_timeout`. Once spent, remaining slots and
    /// siblings are abandoned with a `warn!`.
    async fn connect_pool_backups(
        &mut self,
        config: &Config,
        rrid: &str,
        attempted_initial: &[String],
        live: &std::collections::HashSet<String>,
        timeout_prompt: Option<mtui_hosts::TimeoutPrompt>,
    ) -> Vec<(String, Option<Vec<String>>)> {
        // Snapshot pool state + selection identity before any await.
        let (arbiter, owner, slot_candidates, lock_comment, package_meta) = {
            let base = self.metadata().base();
            (
                base.arbiter,
                base.owner.clone(),
                base.slot_candidates.clone(),
                base.lock_comment.clone(),
                base.packages.clone(),
            )
        };
        let (Some(arbiter), Some(owner)) = (arbiter, owner) else {
            return Vec::new();
        };
        if slot_candidates.is_empty() {
            return Vec::new();
        }
        let store = Self::build_refhosts_store(config).await;

        let wait = i64::try_from(config.lock_wait).unwrap_or(i64::MAX);
        let poll = i64::try_from(config.lock_wait_poll).unwrap_or(i64::MAX);

        let mut attempted: std::collections::HashSet<String> =
            attempted_initial.iter().cloned().collect();
        let mut new_drift: Vec<(String, Option<Vec<String>>)> = Vec::new();

        // Bounds the phase regardless of report size, at the cost of a late slot
        // being starved when earlier slots burn the budget — logged per slot,
        // and strictly better than wedging.
        let started = Instant::now();
        let budget = Duration::from_secs(4 * config.connect_timeout);

        for (slot, candidates) in slot_candidates {
            if started.elapsed() >= budget {
                warn!(
                    slot = %slot,
                    "backup-refhost retry budget exhausted; giving up on remaining slots"
                );
                break;
            }
            if candidates.iter().any(|c| live.contains(c)) {
                continue;
            }
            // Drop dead primary claim(s) so a sibling can be tried and the
            // exhausted-pool wait reflects real availability.
            {
                let base = self.metadata_mut().base_mut();
                for c in &candidates {
                    if base.pool_claims.contains(c) && !live.contains(c) {
                        base.pool_claims.remove(c);
                        arbiter.release(c, &owner);
                    }
                }
            }

            let mut remaining: Vec<String> = candidates
                .iter()
                .filter(|c| !attempted.contains(*c))
                .cloned()
                .collect();
            let mut connected = false;
            while !remaining.is_empty() {
                if started.elapsed() >= budget {
                    warn!(
                        slot = %slot,
                        "backup-refhost retry budget exhausted; giving up on this slot's remaining siblings"
                    );
                    break;
                }
                let Some(chosen) = arbiter.acquire_any(&remaining, &owner, wait, poll).await else {
                    break;
                };
                attempted.insert(chosen.clone());
                remaining.retain(|c| c != &chosen);
                self.metadata_mut()
                    .base_mut()
                    .pool_claims
                    .insert(chosen.clone());
                info!(host = %chosen, slot = %slot, "trying backup refhost for slot");
                match Self::connect_one(
                    config,
                    chosen.clone(),
                    rrid,
                    &timeout_prompt,
                    &lock_comment,
                    &package_meta,
                    store.as_ref(),
                    true, // backup hosts are always pool claims
                )
                .await
                {
                    Some((target, drift_entry)) => {
                        self.targets_mut().add(target);
                        new_drift.push(drift_entry);
                        connected = true;
                        break;
                    }
                    None => {
                        // Free the claim for the next candidate.
                        let base = self.metadata_mut().base_mut();
                        base.pool_claims.remove(&chosen);
                        arbiter.release(&chosen, &owner);
                    }
                }
            }
            if !connected {
                warn!(
                    slot = %slot,
                    candidates = candidates.len(),
                    "no connectable pool host for slot (all candidates tried)"
                );
            }
        }
        new_drift
    }

    /// Compares a freshly connected `target`'s detected products against its
    /// `refhosts.yml` row, returning the per-host warning lines to record.
    ///
    /// `Some(lines)` when [`compare`] against the host's
    /// [`Host`](mtui_types::Product) row reports drift
    /// (base/arch/addon/dangling-symlink; the `qa` addon is always ignored,
    /// inside `compare`). `None` means "no drift, clear any stale entry": the
    /// store is unavailable, the host is absent from `refhosts.yml`, or the
    /// products match. Best-effort — the host is kept regardless.
    fn verify_target_products(store: Option<&Refhosts>, target: &Target) -> Option<Vec<String>> {
        let store = store?;
        let Some(meta) = store.host_by_name(target.hostname()) else {
            tracing::debug!(
                host = %target.hostname(),
                "refhosts.yml has no entry; skipping product check"
            );
            return None;
        };
        let diff = compare(target.system(), meta);
        if diff.ok() {
            return None;
        }
        let lines = diff.warnings();
        for line in &lines {
            warn!(
                host = %target.hostname(),
                "products differ from refhosts.yml metadata: {line}"
            );
        }
        Some(lines)
    }

    /// Applies collected product-drift results to the active report and
    /// surfaces them: `Some(lines)` records drift under the hostname and prints
    /// a yellow warning block, `None` clears any stale entry.
    fn apply_product_warnings(&mut self, drift: Vec<(String, Option<Vec<String>>)>) {
        if drift.is_empty() {
            return;
        }
        for (host, lines) in &drift {
            if let Some(lines) = lines {
                self.display.println(&self.display.yellow(&format!(
                    "{host}: products differ from refhosts.yml metadata:"
                )));
                for line in lines {
                    self.display
                        .println(&self.display.yellow(&format!("  - {line}")));
                }
            }
        }
        let warnings = self.metadata_mut().base_mut();
        for (host, lines) in drift {
            match lines {
                Some(lines) => {
                    warnings.product_warnings.insert(host, lines);
                }
                None => {
                    warnings.product_warnings.remove(&host);
                }
            }
        }
    }

    /// Autolocks a freshly connected `target` with the PI `lock_comment`.
    ///
    /// A no-op when `lock_comment` is empty (no PI assignment active). A host
    /// already locked by another owner is left as-is
    /// ([`HostError::TargetLocked`] suppressed, logged at debug, mirroring
    /// `Target::unlock`); any other lock error is logged at `warn` but never
    /// propagated, so a failed autolock never drops an otherwise-good host.
    async fn autolock_target(target: &mut Target, lock_comment: &str) {
        if lock_comment.is_empty() {
            return;
        }
        match target.lock(lock_comment).await {
            Ok(()) => {}
            Err(HostError::TargetLocked(msg)) => {
                tracing::debug!(host = %target.hostname(), %msg, "autolock: host locked by another owner, leaving as-is");
            }
            Err(e) => {
                warn!(host = %target.hostname(), error = %e, "autolock failed, host still added");
            }
        }
    }

    /// Resolves the active report's testplatforms to candidate hosts (offline)
    /// and connects+adds them to the active group.
    ///
    /// The `add_host`-without-`-t` path: one candidate per matching slot per
    /// testplatform, deduplicated against the group, then connected.
    pub(crate) async fn add_testplatform_hosts(&mut self) {
        let config = self.config.clone();
        let shuffle = self.shuffle;
        let (already, testplatforms, arbiter, owner) = {
            let base = self.metadata().base();
            (
                base.targets.names(),
                base.testplatforms.clone(),
                base.arbiter,
                base.owner.clone(),
            )
        };
        // Same pool-selection path as autoconnect. `ref_hosts` is empty here:
        // `add_host` without `-t` draws purely from the testplatforms.
        let mut wanted = self
            .resolve_and_record_pool(&config, Vec::new(), testplatforms, arbiter, owner, shuffle)
            .await;
        wanted.retain(|h| !already.contains(h));
        wanted.sort();
        wanted.dedup();

        let rrid = self.metadata().id();
        self.connect_and_add_hosts(wanted, &rrid).await;
    }

    /// Connects+adds the explicitly-named `hosts` to the active report's group.
    ///
    /// The `add_host`-with-`-t` path: each host is stamped with the active
    /// report's RRID and connected. One already in the group is warned about and
    /// skipped, matching the silent dedup
    /// [`add_testplatform_hosts`](Self::add_testplatform_hosts) does. The
    /// membership snapshot precedes any `.await`, keeping the future `Send`.
    pub(crate) async fn add_named_hosts(&mut self, hosts: Vec<String>) {
        let already = self.metadata().base().targets.names();
        let mut wanted = Vec::with_capacity(hosts.len());
        for host in hosts {
            if already.contains(&host) {
                warn!(host = %host, "already connected to {host}, skipping");
            } else {
                wanted.push(host);
            }
        }
        let rrid = self.metadata().id();
        self.connect_and_add_hosts(wanted, &rrid).await;
    }

    /// Builds the refhosts inventory on demand from `config`, or `None` on any
    /// resolver/resolve failure.
    ///
    /// Shared by host selection and
    /// [`verify_target_products`](Self::verify_target_products); no cached
    /// Session state, and a `None` result degrades both callers to a no-op.
    /// Takes `&Config` (not `&Session`) so the caller's connect future stays
    /// `Send` across this await.
    async fn build_refhosts_store(config: &Config) -> Option<Refhosts> {
        let factory = match RefhostsFactory::production(
            config.refhosts_path.clone(),
            VerifyPolicy::from_config(&config.ssl_verify),
        ) {
            Ok(f) => f,
            Err(e) => {
                warn!("refhosts resolver init failed: {e}");
                return None;
            }
        };
        match factory
            .resolve(ResolveConfig {
                refhosts_resolvers: &config.refhosts_resolvers,
                refhosts_path: &config.refhosts_path,
                refhosts_https_uri: &config.refhosts_https_uri,
                refhosts_https_expiration: config.refhosts_https_expiration,
                ssl_verify: &config.ssl_verify,
            })
            .await
        {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("refhosts resolve failed: {e}");
                None
            }
        }
    }

    /// Resolves the inventory on demand and searches it per testplatform; a
    /// resolver failure degrades to an empty result.
    ///
    /// Production dispatch always has the arbiter/owner wired (see
    /// [`resolve_and_record_pool`](Self::resolve_and_record_pool)), so this
    /// survives only as the offline baseline
    /// [`autoconnect_hosts_of`](tests::autoconnect_hosts_of) tests pool
    /// selection against.
    #[cfg(test)]
    async fn resolve_testplatform_hosts(config: &Config, testplatforms: &[String]) -> Vec<String> {
        if testplatforms.is_empty() {
            return Vec::new();
        }

        let Some(store) = Self::build_refhosts_store(config).await else {
            return Vec::new();
        };

        let mut hosts: Vec<String> = Vec::new();
        for tp in testplatforms {
            let attrs = Attributes::from_testplatform(tp);
            let found = store.search(&attrs);
            if found.is_empty() {
                info!("autoconnect: nothing found for testplatform {tp:?}");
            }
            for host in found {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        hosts
    }

    /// Pick one distinct free host per test-target slot via the arbiter, run
    /// per testplatform.
    ///
    /// [`search_pool_by_query`](Refhosts::search_pool_by_query) groups
    /// candidates by their *requested* slot (product+version+arch+requested
    /// addons), so hosts interchangeable for the update collapse to one slot.
    /// Each slot's candidates are shuffled (the [`ShuffleFn`] seam) and recorded
    /// so a failed connect can fall back to a sibling; a slot this owner already
    /// holds a host for is skipped; otherwise one free host is claimed through
    /// the arbiter, waiting up to `[lock] wait` seconds when all are busy.
    ///
    /// Returns `(chosen_hosts, slot_candidates)` — this batch's `pool_claims`
    /// and the per-slot ordered candidate lists, keyed as
    /// [`TestReportBase::slot_candidates`](mtui_testreport::TestReportBase::slot_candidates)
    /// — for the caller to write onto the active report before connecting.
    ///
    /// Static (plain data, `&'static` arbiter) so the caller's connect future
    /// stays `Send`.
    async fn pool_select(
        store: &Refhosts,
        testplatforms: &[String],
        arbiter: &'static HostArbiter,
        owner: &Owner,
        wait: i64,
        poll: i64,
        shuffle: ShuffleFn,
    ) -> (Vec<String>, std::collections::HashMap<String, Vec<String>>) {
        use std::collections::HashMap;
        let mut chosen: Vec<String> = Vec::new();
        let mut slot_candidates: HashMap<String, Vec<String>> = HashMap::new();

        for tp in testplatforms {
            let attrs = Attributes::from_testplatform(tp);
            let pairs = store.search_pool_by_query(&attrs);
            if pairs.is_empty() {
                info!("autoconnect: nothing found for testplatform {tp:?}");
                continue;
            }
            // IndexMap keeps insertion order, so grouping is O(pairs) and
            // iteration follows first-seen slot order.
            let mut by_slot: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
            for (host, slot) in pairs {
                by_slot.entry(slot_key(&slot)).or_default().push(host.name);
            }

            for (slot, mut candidates) in by_slot {
                // Spread load across interchangeable hosts, then remember the
                // order for backup-refhost fallback.
                shuffle(&mut candidates);
                slot_candidates.insert(slot.clone(), candidates.clone());

                if candidates
                    .iter()
                    .any(|c| arbiter.owner_of(c).as_ref() == Some(owner))
                {
                    continue;
                }
                match arbiter.acquire_any(&candidates, owner, wait, poll).await {
                    Some(host) => chosen.push(host),
                    None => warn!(
                        slot = %slot,
                        candidates = candidates.len(),
                        "no free pool host for slot (all candidates busy)"
                    ),
                }
            }
        }
        (chosen, slot_candidates)
    }

    /// Combines `ref_hosts` with pool-selected testplatform hosts, records the
    /// pool claims + slot candidates on the active report, and returns the
    /// deduplicated host list to connect.
    ///
    /// Shared by [`autoconnect_active`](Self::autoconnect_active) and
    /// [`add_testplatform_hosts`](Self::add_testplatform_hosts). Each
    /// testplatform contributes one arbiter-chosen host per requested slot (via
    /// [`pool_select`](Self::pool_select)), recorded as `pool_claims` so
    /// [`connect_and_add_hosts`](Self::connect_and_add_hosts) connects only them
    /// (with sibling backup fallback).
    /// [`TemplateRegistry::add`](crate::template_registry::TemplateRegistry::add)
    /// wires the arbiter/owner unconditionally, so an unwired call is
    /// unsupported: it resolves no testplatform hosts and warns, rather than
    /// silently connecting every `search()` match.
    async fn resolve_and_record_pool(
        &mut self,
        config: &Config,
        ref_hosts: Vec<String>,
        testplatforms: Vec<String>,
        arbiter: Option<&'static HostArbiter>,
        owner: Option<Owner>,
        shuffle: ShuffleFn,
    ) -> Vec<String> {
        let mut wanted = ref_hosts;

        let tp_hosts = match (arbiter, owner) {
            (Some(arbiter), Some(owner)) if !testplatforms.is_empty() => {
                if let Some(store) = Self::build_refhosts_store(config).await {
                    let (chosen, slot_candidates) = Self::pool_select(
                        &store,
                        &testplatforms,
                        arbiter,
                        &owner,
                        i64::try_from(config.lock_wait).unwrap_or(i64::MAX),
                        i64::try_from(config.lock_wait_poll).unwrap_or(i64::MAX),
                        shuffle,
                    )
                    .await;
                    // On the active report so `connect_and_add_hosts` connects
                    // only the claims and `quit` can release them.
                    let base = self.metadata_mut().base_mut();
                    for host in &chosen {
                        base.pool_claims.insert(host.clone());
                    }
                    base.slot_candidates.extend(slot_candidates);
                    chosen
                } else {
                    Vec::new()
                }
            }
            (Some(_), Some(_)) => Vec::new(),
            _ => {
                warn!("host arbitration not wired; no testplatform hosts resolved");
                Vec::new()
            }
        };

        for host in tp_hosts {
            if !wanted.contains(&host) {
                wanted.push(host);
            }
        }
        wanted
    }

    /// Requests that the interactive REPL loop exit after the current dispatch.
    ///
    /// Set by `quit`; read by the REPL via [`should_exit`](Self::should_exit).
    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    /// Whether the `quit` command has asked the REPL loop to exit.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Installs the callback `set_log_level` uses to apply a runtime level
    /// change. The REPL wires it to a `tracing_subscriber::reload` handle so the
    /// change takes effect immediately; headless callers leave it unset.
    pub fn set_log_level_sink(&mut self, sink: LogLevelSink) {
        self.log_level_sink = Some(sink);
    }

    /// Applies `level` through the installed sink, if any.
    ///
    /// Returns `true` when a sink was present and invoked; `false` when none is
    /// installed (headless/tests), so the caller can still log the change.
    pub(crate) fn apply_log_level(&mut self, level: LogLevel) -> bool {
        if let Some(sink) = self.log_level_sink.as_mut() {
            sink(level);
            true
        } else {
            false
        }
    }

    /// Installs the callback `notify_user` uses to surface a desktop
    /// notification. The REPL wires it to `mtui-cli`'s
    /// `notification::notify_user`; `mtui-mcp` and tests leave it unset, making
    /// notifications a silent no-op.
    pub fn set_notify_sink(&mut self, sink: NotifySink) {
        self.notify_sink = Some(sink);
    }

    /// Surfaces a best-effort desktop notification through the installed sink, if
    /// any.
    ///
    /// `error` selects the error-class toast. Returns `true` when a sink was
    /// present and invoked, `false` when none is installed (headless/tests).
    pub(crate) fn notify_user(&mut self, msg: &str, error: bool) -> bool {
        if let Some(sink) = self.notify_sink.as_mut() {
            sink(msg, error);
            true
        } else {
            false
        }
    }

    /// Installs the session-level serialised interactive [`Prompter`].
    ///
    /// The composition root (`mtui-cli`'s `main.rs`) wires a
    /// [`Prompter::stdin`](mtui_hosts::Prompter::stdin)-backed prompter here;
    /// `mtui-mcp` leaves it unset. It also reaches the active report's
    /// [`HostsGroup`], so already-connected hosts pick up the derived
    /// command-timeout prompt immediately; later connects inherit it via
    /// `connect_and_add_hosts`.
    pub fn set_prompter(&mut self, prompter: Prompter) {
        self.targets_mut().set_prompter(prompter.clone());
        self.prompter = Some(prompter);
    }

    /// The session-level serialised [`Prompter`], if installed.
    #[must_use]
    pub fn prompter(&self) -> Option<&Prompter> {
        self.prompter.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn fresh_session_active_is_null_and_unloaded() {
        let s = Session::new(config(), true);
        assert!(!s.metadata().is_loaded());
        assert!(s.templates.is_empty());
        assert_eq!(s.metadata().id(), "");
    }

    /// Guards against a regression to per-command client construction: a stable
    /// `ssl_verify` reuses one built client, a posture change rebuilds once.
    #[test]
    fn http_client_is_reused_and_rebuilt_on_posture_change() {
        use mtui_config::SslVerify;

        let mut s = Session::new(config(), true);
        assert_eq!(s.http_builds(), 0, "no client until first use");

        let c0 = s.http_client().expect("client builds");
        for _ in 0..3 {
            let _ = s.http_client().expect("cached clone");
        }
        assert_eq!(s.http_builds(), 1, "one build shared across four calls");
        drop(c0);

        s.config.ssl_verify = SslVerify::Disabled;
        let _ = s.http_client().expect("rebuild under new posture");
        assert_eq!(s.http_builds(), 2, "posture change rebuilds once");
        let _ = s.http_client().expect("cached clone of new posture");
        assert_eq!(s.http_builds(), 2, "no rebuild while posture stable");
    }

    #[test]
    fn is_repl_flag_is_honored() {
        assert!(Session::new(config(), true).is_repl);
        assert!(!Session::new(config(), false).is_repl);
    }

    #[test]
    fn targets_of_unloaded_session_is_empty() {
        let s = Session::new(config(), true);
        assert!(s.targets().is_empty());
    }

    /// Seeds a registry entry under `rrid` owning one mock host, so a teardown
    /// enumeration can be attributed per unit by hostname.
    fn seed_report_with_host(session: &mut Session, rrid: &str, host: &str) {
        let mut report = ObsReport::new(session.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        report.base_mut().targets = HostsGroup::new(vec![mock_target(host)], false);
        session.templates.add(Box::new(report));
    }

    /// Every registry entry **and** the sentinel's stranded group, each exactly
    /// once. Exactly-once cannot be read off the wire (a second `Target::close`
    /// is a designed no-op), so it is pinned here, where handing the sentinel
    /// out twice is observable.
    #[tokio::test]
    async fn take_teardown_units_cover_registry_and_null_group_once() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        seed_report_with_host(&mut s, "SUSE:Maintenance:2:2", "t2");
        s.activate("SUSE:Maintenance:1:1");

        // Plant a host in the sentinel's group (what `add_host` writes into with
        // nothing loaded), then restore the guard teardown runs under.
        s.release_active_guard();
        s.targets_mut().add(mock_target("n1"));
        s.refresh_active_guard();
        assert!(
            s.active_report_is_guarded("SUSE:Maintenance:1:1"),
            "fixture must hold the active guard, or the release below proves nothing"
        );

        let handles = s.take_teardown_units();
        assert_eq!(handles.len(), 3, "two registry entries plus the null unit");
        for (i, a) in handles.iter().enumerate() {
            for b in &handles[i + 1..] {
                assert!(!std::sync::Arc::ptr_eq(a, b), "units must be distinct");
            }
        }
        let mut named = Vec::new();
        for h in &handles {
            let report = h
                .try_lock()
                .expect("the seam released the active guard: no self-deadlock");
            named.push(report.base().targets.names());
        }
        assert_eq!(
            named,
            vec![vec!["n1"], vec!["t1"], vec!["t2"]],
            "the null unit is stranded past a shared deadline, so it goes first"
        );

        assert_eq!(
            s.take_teardown_units().len(),
            2,
            "the sentinel is handed out once: a re-entrant teardown finds it fresh and empty"
        );
    }

    /// Nothing loaded, no hosts anywhere: hand out nothing (not an empty null
    /// unit) and leave the session usable.
    #[tokio::test]
    async fn take_teardown_units_empty_session_yields_nothing() {
        let mut s = Session::new(config(), false);
        assert!(s.take_teardown_units().is_empty());
        assert!(!s.metadata().is_loaded());
        assert!(s.targets().is_empty());
    }

    #[test]
    fn prompter_is_none_until_installed_then_some() {
        let mut s = Session::new(config(), true);
        assert!(s.prompter().is_none());
        let p = mtui_hosts::Prompter::new(std::sync::Arc::new(|_t: String| {
            Box::pin(async move { Ok(String::new()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }));
        s.set_prompter(p);
        assert!(s.prompter().is_some());
    }

    #[test]
    fn log_level_parse_and_tracing_mapping() {
        assert_eq!(LogLevel::parse("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warning));
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("bogus"), None);
        assert_eq!(LogLevel::Debug.as_tracing(), tracing::Level::DEBUG);
        assert_eq!(LogLevel::Error.as_tracing(), tracing::Level::ERROR);
    }

    #[test]
    fn apply_log_level_invokes_sink_when_installed() {
        use std::sync::{Arc, Mutex};
        let mut s = Session::new(config(), true);
        assert!(!s.apply_log_level(LogLevel::Debug));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        s.set_log_level_sink(Box::new(move |lvl| sink_seen.lock().unwrap().push(lvl)));
        assert!(s.apply_log_level(LogLevel::Warning));
        assert_eq!(*seen.lock().unwrap(), vec![LogLevel::Warning]);
    }

    #[test]
    fn with_display_uses_supplied_sink() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let s = Session::with_display(config(), false, display);
        assert_eq!(s.display.color(), ColorMode::Always);
        assert!(!s.is_repl);
    }

    #[test]
    fn new_display_defaults_never_but_set_color_applies() {
        // The production `mtui-cli::main` seam: a stdout display defaulting to
        // `Never`, then `--color` applied via `set_color`. Guards against the
        // resolved mode never reaching the display, so colors never appear.
        use crate::display::ColorMode;
        let mut s = Session::new(config(), true);
        assert_eq!(s.display.color(), ColorMode::Never);
        assert!(!s.display.color().resolve());

        s.display.set_color(ColorMode::Always);
        assert_eq!(s.display.color(), ColorMode::Always);
        assert!(s.display.color().resolve());

        s.display.set_color(ColorMode::Never);
        assert!(!s.display.color().resolve());
    }

    // --- load_update + autoconnect host resolution -------------------------

    use mtui_hosts::MockConnection;
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;

    const REFHOSTS_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mtui-datasources/tests/fixtures/refhosts.yml"
    );

    /// A config whose refhosts resolver is the offline file-backed `path`
    /// resolver pointed at the fixture above (no network).
    fn config_with_path_refhosts() -> Config {
        let mut c = Config::default();
        c.refhosts_resolvers = "path".to_owned();
        c.refhosts_path = REFHOSTS_FIXTURE.into();
        c
    }

    /// Adds an active `ObsReport` with the given reference hostnames and
    /// testplatforms to `session`.
    fn seed_active_report(
        session: &mut Session,
        rrid: &str,
        hostnames: &[&str],
        testplatforms: &[&str],
    ) {
        let mut report = ObsReport::new(session.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        for h in hostnames {
            report.base_mut().hostnames.insert((*h).to_owned());
        }
        report.base_mut().testplatforms = testplatforms.iter().map(|s| (*s).to_owned()).collect();
        session.templates.add(Box::new(report));
        session.activate(rrid);
    }

    /// The pre-pool autoconnect host set: reference hosts merged with the
    /// `search()`-resolved testplatform hosts, minus the already-connected ones.
    /// A comparison baseline for [`Session::resolve_and_record_pool`]'s
    /// pool selection, never a path production dispatch takes.
    async fn autoconnect_hosts_of(s: &Session) -> Vec<String> {
        let config = s.config.clone();
        let (ref_hosts, already, testplatforms) = {
            let base = s.metadata().base();
            (
                base.hostnames.iter().cloned().collect::<Vec<_>>(),
                base.targets.names(),
                base.testplatforms.clone(),
            )
        };
        let mut wanted = ref_hosts;
        wanted.sort();
        wanted.dedup();
        for host in Session::resolve_testplatform_hosts(&config, &testplatforms).await {
            if !wanted.contains(&host) {
                wanted.push(host);
            }
        }
        wanted.retain(|h| !already.contains(h));
        wanted
    }

    /// Reference hosts combined with the hosts resolved from the testplatforms
    /// (offline `path` resolver), sorted and deduplicated.
    #[tokio::test]
    async fn autoconnect_hosts_merges_reference_and_testplatform_hosts() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        // The fixture's minor is the numeric `5`, so the query needs `minor=5`.
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-a.example.com"],
            &["base=sles(major=15,minor=5);arch=[x86_64]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;

        assert!(hosts.contains(&"ref-a.example.com".to_owned()));
        assert!(
            hosts.iter().any(|h| h.contains("x86")),
            "expected a resolved x86 refhost, got: {hosts:?}"
        );
        let mut sorted = hosts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), hosts.len(), "hosts must be deduplicated");
    }

    /// With no testplatforms the result is exactly the reference-host set.
    #[tokio::test]
    async fn autoconnect_hosts_reference_only_when_no_testplatforms() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &["only.example.com"], &[]);

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["only.example.com".to_owned()]);
    }

    /// A testplatform matching nothing in the inventory contributes no hosts;
    /// the reference hosts still stand.
    #[tokio::test]
    async fn autoconnect_hosts_unmatched_testplatform_yields_reference_only() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-only.example.com"],
            &["base=sles(major=99,minor=sp9);arch=[nonesuch]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["ref-only.example.com".to_owned()]);
    }

    /// A kernel update loads the on-disk template and activates it without
    /// autoconnecting, so a load never touches a live host.
    #[tokio::test]
    async fn load_update_kernel_loads_and_activates_without_connect() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();

        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        let loaded = s.load_update(&update, true, UpdateKind::Kernel).await;

        assert_eq!(loaded, rrid);
        assert!(s.templates.contains(rrid));
        assert_eq!(s.templates.active_rrid(), Some(rrid));
        assert!(s.targets().is_empty());
    }

    /// A Product Increment under the default `lock_pi_autolock` seeds
    /// `lock_comment` before any host connects: the PI is locked for the life of
    /// the loaded report, not bracketed around `assign`/`approve`.
    #[tokio::test]
    async fn load_update_reported_seeds_pi_lock_comment_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:PI:1.2:5";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();

        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        assert!(config.lock_pi_autolock, "default must be enabled");
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel).await;

        assert_eq!(
            s.metadata().base().lock_comment,
            format!("testing of {rrid}")
        );
    }

    /// A Maintenance RRID never gets the PI lock comment, whatever
    /// `lock_pi_autolock` says.
    #[tokio::test]
    async fn load_update_reported_leaves_lock_comment_empty_for_maintenance_rrid() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();

        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        assert!(config.lock_pi_autolock, "default must be enabled");
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel).await;

        assert_eq!(s.metadata().base().lock_comment, "");
    }

    /// `lock_pi_autolock = false` leaves the PI's `lock_comment` empty.
    #[tokio::test]
    async fn load_update_reported_leaves_lock_comment_empty_when_autolock_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:PI:1.2:5";
        let dir = tmp.path().join(rrid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), "log\n").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!("{{\"rrid\": \"{rrid}\", \"repository\": \"http://x/\"}}"),
        )
        .unwrap();

        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        config.lock_pi_autolock = false;
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel).await;

        assert_eq!(s.metadata().base().lock_comment, "");
    }

    /// An unloadable RRID falls back to the null report: nothing registered,
    /// empty RRID returned.
    #[tokio::test]
    async fn load_update_missing_report_returns_empty_and_registers_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        // Make the internal `svn co` fail fast offline.
        config.svn_path = format!("file://{}/no-such-repo", tmp.path().display());
        let mut s = Session::new(config, false);

        let update = UpdateID::parse("SUSE:Maintenance:1:1").unwrap();
        let (loaded, reason) = s
            .load_update_reported(&update, true, UpdateKind::Auto)
            .await;

        assert_eq!(loaded, "");
        assert!(s.templates.is_empty());
        let reason = reason.expect("a failed load should report a reason");
        assert!(
            reason.contains("svn checkout"),
            "reason should name the underlying cause: {reason}"
        );
    }

    /// `set_workflow` mutates the active report's workflow mode.
    #[test]
    fn set_workflow_mutates_active_report() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        assert_eq!(s.metadata().workflow(), Workflow::Manual);
        s.set_workflow(Workflow::Auto);
        assert_eq!(s.metadata().workflow(), Workflow::Auto);
    }

    /// An unreachable host fails its live connect and is skipped, not added.
    #[tokio::test]
    async fn add_named_hosts_skips_unconnectable() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        s.add_named_hosts(vec!["unreachable.invalid".to_owned()])
            .await;
        assert!(s.targets().is_empty());
    }

    /// A host already in the active group is warned about and dropped before the
    /// connect loop, leaving the group size unchanged.
    #[tokio::test]
    async fn add_named_hosts_skips_already_connected() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        s.targets_mut().add(mock_target("refhost.example"));
        assert_eq!(s.targets().len(), 1);
        assert!(s.targets().contains("refhost.example"));

        s.add_named_hosts(vec!["refhost.example".to_owned()]).await;

        assert_eq!(
            s.targets().len(),
            1,
            "already-connected host must not be re-added"
        );
    }

    /// Testplatforms resolve via the offline `path` resolver and are then
    /// connected; the unreachable fixture hosts are skipped, but the resolution
    /// path is exercised.
    #[tokio::test]
    async fn add_testplatform_hosts_resolves_and_connects() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &[],
            &["base=sles(major=15,minor=5);arch=[x86_64]"],
        );
        s.add_testplatform_hosts().await;
        assert!(s.targets().is_empty());
    }

    /// With no testplatforms, `add_testplatform_hosts` is a no-op.
    #[tokio::test]
    async fn add_testplatform_hosts_no_testplatforms_is_noop() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        s.add_testplatform_hosts().await;
        assert!(s.targets().is_empty());
    }

    /// Regression (spinner invisible during `update`): `take_targets` /
    /// `split_targets` must propagate the session's `is_repl` to the taken group
    /// and both split halves, or the fan-out spinner/prompt seam is silently
    /// suppressed on the perform_* path.
    #[tokio::test]
    async fn take_and_split_targets_propagate_session_is_repl() {
        let mut s = Session::new(config_with_path_refhosts(), true);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        // The load-time reconcile `make_testreport` performs.
        s.targets_mut().set_is_repl(true);
        s.targets_mut().add(mock_target("refhost.example"));

        let taken = s.take_targets();
        assert!(
            taken.is_repl(),
            "take_targets must hand back an is_repl=true group"
        );
        s.restore_targets(taken);

        let (selected, remainder) = s.split_targets(None, true).expect("split");
        assert!(selected.is_repl(), "split selected half must be is_repl");
        assert!(remainder.is_repl(), "split remainder half must be is_repl");
    }

    /// A mock-backed, already-connected [`Target`]: the seam the connect loop
    /// reaches once `Target::connect` short-circuits.
    fn mock_target(host: &str) -> Target {
        Target::with_connection(
            host,
            TargetState::Enabled,
            Box::new(MockConnection::new(host)),
        )
    }

    /// `autolock_target` locks a freshly connected host with the PI comment when
    /// a `lock_comment` is active.
    #[tokio::test]
    async fn autolock_target_locks_when_comment_set() {
        let mut t = mock_target("refhost.example");
        assert!(!t.is_locked().await.expect("is_locked before"));
        Session::autolock_target(&mut t, "mtui pool SUSE:Maintenance:1:1 alice").await;
        assert!(
            t.is_locked().await.expect("is_locked after"),
            "host should be locked after autolock with a non-empty comment"
        );
    }

    /// With an empty `lock_comment` (no PI assignment active), `autolock_target`
    /// is a no-op: the host is left unlocked.
    #[tokio::test]
    async fn autolock_target_noop_when_comment_empty() {
        let mut t = mock_target("refhost.example");
        Session::autolock_target(&mut t, "").await;
        assert!(
            !t.is_locked().await.expect("is_locked"),
            "host must not be locked when no PI assignment is active"
        );
    }

    /// A host locked by another owner is left as-is: the foreign
    /// [`HostError::TargetLocked`] is suppressed.
    #[tokio::test]
    async fn autolock_target_suppresses_foreign_lock() {
        // A fresh foreign lock file (huge future pid, distinct user) so the
        // mock's lock read sees another owner and refuses to relock.
        let conn = MockConnection::new("refhost.example").with_file(
            "/var/lock/mtui.lock",
            format!("{}:someone-else:2147483647", i64::MAX),
        );
        let mut t =
            Target::with_connection("refhost.example", TargetState::Enabled, Box::new(conn));
        Session::autolock_target(&mut t, "mtui pool SUSE:Maintenance:1:1 alice").await;
    }

    /// A pool-claimed host under a non-empty `lock_comment` holds **both** the
    /// pool claim and the operation lock, pinning that the two branches are
    /// sequential rather than the `if/else` that left a pool host never
    /// PI-locked.
    #[tokio::test]
    async fn lock_connected_target_pool_claim_and_pi_lock_are_not_exclusive() {
        let mut t = mock_target("refhost.example");
        let ok = Session::lock_connected_target(
            &mut t,
            "refhost.example",
            "SUSE:Maintenance:1:1",
            "testing of SUSE:Maintenance:1:1",
            true, // is_pool_claim
        )
        .await;
        assert!(ok, "a free host must be claimable");
        assert!(
            t.is_locked().await.expect("is_locked"),
            "a pool-claimed host must also hold the PI operation lock"
        );
    }

    // --- product-drift verification -----

    use mtui_types::system::{System, SystemProduct};
    use mtui_types::{Host, Product};
    use std::collections::BTreeSet;

    /// A [`Target`] carrying a detected [`System`] (base product + addons).
    fn mock_target_with_system(
        host: &str,
        base: SystemProduct,
        addons: &[SystemProduct],
    ) -> Target {
        let mut t = mock_target(host);
        let addons: BTreeSet<SystemProduct> = addons.iter().cloned().collect();
        t.set_system(System::new(base, addons, false), false);
        t
    }

    /// A single-row refhosts store: host `name`, sles `major.minor` on `arch`.
    fn store_with_sles(name: &str, major: u64, minor: u64, arch: &str) -> Refhosts {
        use mtui_types::version::{Version, VersionField};
        Refhosts::from_hosts(vec![Host {
            name: name.to_owned(),
            arch: arch.to_owned(),
            product: Product {
                name: "sles".to_owned(),
                version: Some(Version::new(major, Some(VersionField::Num(minor)))),
            },
            addons: Vec::new(),
        }])
    }

    /// Matching products yield no warnings (`None` clears any stale entry).
    #[test]
    fn verify_target_products_none_on_match() {
        let store = store_with_sles("host.example", 15, 5, "x86_64");
        let t = mock_target_with_system(
            "host.example",
            SystemProduct::new("sles", "15.5", "x86_64"),
            &[],
        );
        assert!(Session::verify_target_products(Some(&store), &t).is_none());
    }

    /// A host whose base product drifts from its row yields warning lines.
    #[test]
    fn verify_target_products_reports_base_drift() {
        let store = store_with_sles("host.example", 15, 5, "x86_64");
        let t = mock_target_with_system(
            "host.example",
            SystemProduct::new("sles", "15.4", "x86_64"),
            &[],
        );
        let lines =
            Session::verify_target_products(Some(&store), &t).expect("drift should be reported");
        assert!(!lines.is_empty());
        assert!(
            lines.iter().any(|l| l.contains("base product mismatch")),
            "expected a base-product mismatch line, got {lines:?}"
        );
    }

    /// A host absent from `refhosts.yml` is skipped silently (`None`).
    #[test]
    fn verify_target_products_none_when_host_absent() {
        let store = store_with_sles("other.example", 15, 5, "x86_64");
        let t = mock_target_with_system(
            "host.example",
            SystemProduct::new("sles", "15.5", "x86_64"),
            &[],
        );
        assert!(Session::verify_target_products(Some(&store), &t).is_none());
    }

    /// A `None` store (refhosts unavailable) disables the check entirely.
    #[test]
    fn verify_target_products_none_when_store_missing() {
        let t = mock_target_with_system(
            "host.example",
            SystemProduct::new("sles", "15.4", "x86_64"),
            &[],
        );
        assert!(Session::verify_target_products(None, &t).is_none());
    }

    /// The `qa` addon is always ignored (inside `compare`), so a host carrying
    /// only an extra `qa` over its row still matches.
    #[test]
    fn verify_target_products_ignores_qa_addon() {
        let store = store_with_sles("host.example", 15, 5, "x86_64");
        let t = mock_target_with_system(
            "host.example",
            SystemProduct::new("sles", "15.5", "x86_64"),
            &[SystemProduct::new("qa", "15.5", "x86_64")],
        );
        assert!(
            Session::verify_target_products(Some(&store), &t).is_none(),
            "qa addon must not be treated as drift"
        );
    }

    /// `apply_product_warnings` records drift under the hostname and clears a
    /// stale entry for a host that now matches.
    #[test]
    fn apply_product_warnings_records_and_clears() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        // A stale entry the later match must clear.
        s.metadata_mut()
            .base_mut()
            .product_warnings
            .insert("stale.example".to_owned(), vec!["old".to_owned()]);

        s.apply_product_warnings(vec![
            (
                "drift.example".to_owned(),
                Some(vec!["base product mismatch: x".to_owned()]),
            ),
            ("stale.example".to_owned(), None),
        ]);

        let base = s.metadata().base();
        assert_eq!(
            base.product_warnings
                .get("drift.example")
                .map(Vec::as_slice),
            Some(["base product mismatch: x".to_owned()].as_slice())
        );
        assert!(
            !base.product_warnings.contains_key("stale.example"),
            "a matching host must clear its stale product_warnings entry"
        );
    }

    // --- pool selection ----------------------------------------------------

    use mtui_types::version::{Version, VersionField};

    /// A refhosts store with several `sles major.minor arch` hosts (no addons).
    fn multi_host_store(rows: &[(&str, u64, u64, &str)]) -> Refhosts {
        Refhosts::from_hosts(
            rows.iter()
                .map(|(name, major, minor, arch)| Host {
                    name: (*name).to_owned(),
                    arch: (*arch).to_owned(),
                    product: Product {
                        name: "sles".to_owned(),
                        version: Some(Version::new(*major, Some(VersionField::Num(*minor)))),
                    },
                    addons: Vec::new(),
                })
                .collect(),
        )
    }

    /// A leaked, empty process-local arbiter: the `&'static` the pool API
    /// expects, without touching the shared global singleton.
    fn test_arbiter() -> &'static HostArbiter {
        Box::leak(Box::new(HostArbiter::new()))
    }

    /// Identity shuffle so pool selection is deterministic in tests.
    fn no_shuffle(_c: &mut [String]) {}

    /// Interchangeable hosts (same requested slot) collapse to one arbiter-chosen
    /// host; distinct arches stay distinct slots.
    #[tokio::test]
    async fn pool_select_one_host_per_requested_slot() {
        let store = multi_host_store(&[
            ("x86-a", 15, 5, "x86_64"),
            ("x86-b", 15, 5, "x86_64"),
            ("ppc-a", 15, 5, "ppc64le"),
        ]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64,ppc64le]".to_owned()];

        let (chosen, slot_candidates) =
            Session::pool_select(&store, &tps, arbiter, &owner, 0, 0, no_shuffle).await;

        assert_eq!(
            chosen.len(),
            2,
            "expected one host per slot, got {chosen:?}"
        );
        assert_eq!(slot_candidates.len(), 2, "two distinct slots recorded");
        let x86_slot = slot_candidates
            .values()
            .find(|c| c.contains(&"x86-a".to_owned()) || c.contains(&"x86-b".to_owned()))
            .expect("x86 slot present");
        assert_eq!(
            x86_slot.len(),
            2,
            "both x86 hosts kept as backup candidates"
        );
        // Deterministic shuffle → first candidate per slot.
        assert!(chosen.contains(&"x86-a".to_owned()));
        assert!(chosen.contains(&"ppc-a".to_owned()));
    }

    /// A slot already held by this owner (across testplatforms) is not
    /// re-claimed — the arbiter hands out one host per owner per slot.
    #[tokio::test]
    async fn pool_select_skips_slot_owner_already_holds() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64"), ("x86-b", 15, 5, "x86_64")]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        // Pre-claim one candidate of the (only) slot for this owner.
        assert!(arbiter.try_acquire("x86-a", &owner));
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()];

        let (chosen, _) =
            Session::pool_select(&store, &tps, arbiter, &owner, 0, 0, no_shuffle).await;

        assert!(
            chosen.is_empty(),
            "owner already holds the slot; no new claim expected, got {chosen:?}"
        );
    }

    /// A slot whose every candidate is held by a *different* owner yields no
    /// host (fail-fast with wait=0), and is warned about — not connected.
    #[tokio::test]
    async fn pool_select_no_free_host_when_all_busy() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64"), ("x86-b", 15, 5, "x86_64")]);
        let arbiter = test_arbiter();
        let mine: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        // Another owner holds both candidates.
        assert!(arbiter.try_acquire("x86-a", &other));
        assert!(arbiter.try_acquire("x86-b", &other));
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()];

        let (chosen, slot_candidates) =
            Session::pool_select(&store, &tps, arbiter, &mine, 0, 0, no_shuffle).await;

        assert!(chosen.is_empty(), "all candidates busy → no claim");
        // Candidates are still recorded (for backup once one frees up).
        assert_eq!(slot_candidates.len(), 1);
    }

    /// Hosts are claimed in the order their slots first appear in the
    /// `search_pool_by_query` output (arch fan-out order). Guards against an
    /// accidental switch to an unordered map.
    #[tokio::test]
    async fn pool_select_preserves_first_seen_slot_order() {
        // Both the store rows and the arch list lead with ppc, so the first-seen
        // slot order is ppc → x86, not alphabetical.
        let store = multi_host_store(&[
            ("ppc-a", 15, 5, "ppc64le"),
            ("x86-a", 15, 5, "x86_64"),
            ("ppc-b", 15, 5, "ppc64le"),
            ("x86-b", 15, 5, "x86_64"),
        ]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=15,minor=5);arch=[ppc64le,x86_64]".to_owned()];

        let (chosen, slot_candidates) =
            Session::pool_select(&store, &tps, arbiter, &owner, 0, 0, no_shuffle).await;

        assert_eq!(
            chosen,
            vec!["ppc-a".to_owned(), "x86-a".to_owned()],
            "chosen hosts must follow first-seen slot order"
        );
        assert_eq!(slot_candidates.len(), 2, "two distinct arch slots");
    }

    /// With the arbiter/owner unwired, no testplatform host is resolved at all —
    /// pinning that the connect-every-`search()`-match fallback is gone.
    /// Production always wires both, so this only exercises the fall-through.
    #[tokio::test]
    async fn resolve_and_record_pool_unwired_resolves_no_testplatform_hosts() {
        let config = config_with_path_refhosts();
        let mut s = Session::new(config.clone(), false);
        let ref_hosts = vec!["explicit-host".to_owned()];
        // Matches fixture hosts a wired arbiter/owner would otherwise resolve.
        let testplatforms = vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()];

        let wanted = s
            .resolve_and_record_pool(
                &config,
                ref_hosts.clone(),
                testplatforms,
                None,
                None,
                no_shuffle,
            )
            .await;

        assert_eq!(
            wanted, ref_hosts,
            "unwired arbiter/owner must not resolve any testplatform host"
        );
    }

    /// Once the shared budget is spent, the remaining slots are abandoned rather
    /// than every sibling of every slot walked. Each candidate is a black-hole
    /// listener (bound, never `accept()`ed) costing a full `connect_timeout = 1s`
    /// to fail, so walking all 6 across 3 slots would take ~6s while the
    /// `4 * connect_timeout` budget must cut it off around 4s.
    #[tokio::test]
    async fn connect_pool_backups_stops_once_budget_is_spent() {
        let mut config = config_with_path_refhosts();
        config.connect_timeout = 1;
        let mut s = Session::new(config.clone(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);

        // 3 slots x 2 siblings, kept alive so the ports stay open.
        let listeners: Vec<std::net::TcpListener> = (0..6)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind"))
            .collect();
        let candidates: Vec<String> = listeners
            .iter()
            .map(|l| format!("127.0.0.1:{}", l.local_addr().unwrap().port()))
            .collect();

        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner);
            for (i, pair) in candidates.chunks(2).enumerate() {
                base.slot_candidates
                    .insert(format!("slot-{i}"), pair.to_vec());
            }
        }

        let live = std::collections::HashSet::new();
        let started = Instant::now();
        let drift = tokio::time::timeout(
            Duration::from_secs(7),
            s.connect_pool_backups(&config, "SUSE:Maintenance:1:1", &[], &live, None),
        )
        .await
        .expect("must not hang past the outer test wrapper");

        assert!(drift.is_empty(), "no black hole ever connects: {drift:?}");
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "shared budget must cut the walk short of trying all 6 siblings \
             (~6s naive), took {:?}",
            started.elapsed()
        );
        drop(listeners);
    }

    /// `check_cancelled` is `Ok` until the token fires, then reports
    /// [`CommandError::Cancelled`]; [`Session::set_cancel_token`] *replaces*
    /// (never chains) the token.
    #[test]
    fn cancel_seam_check_and_replace() {
        let mut s = Session::new(Config::default(), false);
        assert!(!s.cancel_requested());
        assert!(s.check_cancelled().is_ok());

        s.cancel_token().cancel();
        assert!(s.cancel_requested());
        assert!(matches!(
            s.check_cancelled(),
            Err(CommandError::Cancelled(_))
        ));

        // The MCP self-healing install path relies on this replacement.
        s.set_cancel_token(tokio_util::sync::CancellationToken::new());
        assert!(!s.cancel_requested());
        assert!(s.check_cancelled().is_ok());
    }

    /// `fork_for_call` clones the token — cancellation state is shared across
    /// the fork in both directions (the MCP job layer installs the per-job
    /// token on the fork and cancels it from `job_cancel`).
    #[test]
    fn fork_for_call_shares_cancellation_state() {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let s = Session::new(Config::default(), false);
        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);
        let fork = s.fork_for_call(display);

        assert!(!fork.cancel_requested());
        s.cancel_token().cancel();
        assert!(fork.cancel_requested(), "fork shares the parent's token");
        assert!(s.cancel_requested());
    }

    /// `fork_for_call` shares the canonical session's loaded reports (same entry
    /// locks) while carrying its own display, so a per-RRID command dispatched on
    /// a fork mutates the *shared* report content visible to the canonical
    /// session.
    #[test]
    fn fork_for_call_shares_reports_with_own_display() {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);

        // The canonical session must not hold a guard on the entry the fork will
        // lock — the MCP `run_command` exclusive path releases it after each
        // call for exactly this reason.
        s.release_active_guard();

        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let mut fork = s.fork_for_call(display);
        assert_eq!(fork.display.color(), ColorMode::Always);
        assert!(fork.activate("SUSE:Maintenance:1:1"));
        fork.set_workflow(Workflow::Auto);
        // So the canonical read below can lock the shared entry.
        fork.release_active_guard();
        drop(fork);

        let entry = s.templates.handle("SUSE:Maintenance:1:1").expect("entry");
        let report = entry.try_lock().expect("uncontended");
        assert_eq!(
            report.base().workflow,
            Workflow::Auto,
            "fork mutation is visible on the shared report"
        );
    }

    /// The composition root wires the arbiter + owner onto every added report
    /// (`_pool_selection_active`), so autoconnect takes the pool path.
    #[test]
    fn added_report_has_arbiter_and_owner_wired() {
        let mut s = Session::new(config(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let base = s.metadata().base();
        assert!(base.arbiter.is_some(), "arbiter must be wired on add()");
        let owner = base.owner.as_ref().expect("owner wired");
        assert_eq!(
            owner.1, "SUSE:Maintenance:1:1",
            "owner RRID is the report id"
        );
    }
}
