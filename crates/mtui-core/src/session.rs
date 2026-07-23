//! Shared, explicitly-passed command state (`Session`).
//!
//! The Rust replacement for upstream's `CommandPrompt` god-object. Commands
//! receive `&mut Session` and read/mutate its state through methods — there are
//! no hidden globals. It owns the [`Config`], the [`TemplateRegistry`] (loaded
//! templates + active pointer), the [`CommandPromptDisplay`] output sink, and
//! the `interactive` flag that distinguishes the REPL (`true`) from headless
//! callers such as `mtui-mcp` (`false`).
//!
//! The scalar `metadata` / `targets` accessors upstream exposes as read-only
//! properties are provided here as [`metadata`](Session::metadata) /
//! [`targets`](Session::targets), delegating to the active report so command
//! bodies and tests keep working as the registry grows past one entry.

use std::sync::Mutex;

use mtui_config::Config;
use mtui_datasources::HttpError;
use mtui_datasources::http::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_datasources::refhost::{Attributes, Refhosts, RefhostsFactory, ResolveConfig, compare};
use mtui_hosts::{HostArbiter, HostError, HostsGroup, Owner, Prompter, Target};
use mtui_testreport::{NullReport, TestReport, UpdateKind, make_testreport};
use mtui_types::UpdateID;
use mtui_types::enums::{ExecutionMode, TargetState, Workflow};
use tokio::sync::OwnedMutexGuard;
use tracing::{info, warn};

use crate::display::CommandPromptDisplay;
use crate::template_registry::TemplateRegistry;

/// The explicitly-passed state every command operates on.
pub struct Session {
    /// The application configuration.
    pub config: Config,
    /// Loaded templates and the active pointer.
    pub templates: TemplateRegistry,
    /// The per-call active-report handle: the [`OwnedMutexGuard`] of the entry
    /// this dispatch is acting on (`mtui-rs-f36r`, step 2).
    ///
    /// [`metadata`](Self::metadata) / [`targets`](Self::targets) and their `_mut`
    /// counterparts read through this guard when present, and through
    /// [`null`](Self::null) when nothing is loaded — so command bodies keep the
    /// *unchanged* sync `session.metadata()` / `session.targets()` surface. The
    /// fan-out driver [`Command::run`](crate::Command::run) installs it per
    /// resolved template (saving/restoring the prior active). Installing it means
    /// dropping any prior guard first (releasing that entry's lock) before
    /// acquiring the new one, so the same [`Session`] never self-deadlocks on one
    /// entry; while the MCP outer session mutex is still in place (steps 1-3) the
    /// entry locks are uncontended and behaviour is preserved.
    active_guard: Option<OwnedMutexGuard<Box<dyn TestReport + Send + Sync>>>,
    /// The null-object fallback [`metadata`](Self::metadata) hands out when
    /// nothing is loaded (no [`active_guard`](Self::active_guard) installed).
    null: Box<dyn TestReport + Send + Sync>,
    /// Formatted-output sink.
    pub display: CommandPromptDisplay,
    /// `true` for the interactive REPL, `false` for headless callers (MCP).
    ///
    /// Drives the fan-out default: with several templates loaded and no
    /// interactive `switch` to pick an active one, an otherwise-unscoped command
    /// fans out across every template instead of silently picking one.
    pub is_repl: bool,
    /// Set by the `quit` command to ask the interactive REPL loop to exit after
    /// the current dispatch returns.
    ///
    /// The Rust replacement for upstream `Quit` raising `SystemExit`/returning a
    /// truthy value from `onecmd`: rather than routing process-exit through the
    /// command error channel, `quit` flips this flag and returns `Ok(())`; the
    /// Phase-6 REPL checks [`should_exit`](Self::should_exit) after each line and
    /// breaks its loop. Headless callers (MCP) ignore it.
    should_exit: bool,
    /// Optional sink for runtime log-level changes (upstream
    /// `prompt.log.setLevel`).
    ///
    /// `set_log_level` calls this with the requested [`LogLevel`] when present.
    /// The Phase-6 REPL installs a callback backed by a
    /// `tracing_subscriber::reload` handle; headless callers and tests leave it
    /// `None`, so the command still logs the change but mutates nothing.
    log_level_sink: Option<LogLevelSink>,
    /// Optional sink for best-effort desktop notifications (upstream
    /// `prompt.notify_user`).
    ///
    /// [`notify_user`](Self::notify_user) calls this with the message and an
    /// error flag when present. The Phase-6 REPL installs a callback backed by
    /// `mtui-cli`'s `notification::notify_user` (a headless no-op); headless
    /// callers (`mtui-mcp`) and tests leave it `None`, so a command that fires a
    /// toast silently does nothing — keeping notifications a REPL-only courtesy
    /// and `mtui-core` free of any dependency on the CLI notification backend.
    notify_sink: Option<NotifySink>,
    /// The session-level serialised interactive [`Prompter`], or `None` under
    /// headless callers (`mtui-mcp`).
    ///
    /// The composition root (`mtui-cli`'s `main.rs`) installs a
    /// [`Prompter::stdin`]-backed prompter via [`set_prompter`](Self::set_prompter)
    /// for the REPL; `mtui-mcp` leaves it unset (upstream `prompter=None`). It is
    /// pushed down two ways: the command-timeout prompt onto each freshly-built
    /// [`Target`] in [`connect_and_add_hosts`](Self::connect_and_add_hosts), and
    /// onto the active report's [`HostsGroup`] via
    /// [`HostsGroup::set_prompter`] (for the serial-barrier Enter prompt). When
    /// `None`, a command timeout aborts immediately and serial hosts run
    /// back-to-back.
    prompter: Option<Prompter>,
    /// Test-only count of how many times [`http_client`](Self::http_client)
    /// actually *built* a client (vs. handing back a cached clone). The
    /// regression oracle for perf bead `mtui-rs-0mop.13`: proves back-to-back
    /// calls under stable config reuse one client, and that a mid-session
    /// posture change rebuilds exactly once.
    #[cfg(test)]
    http_builds: std::sync::atomic::AtomicUsize,
    /// Per-slot candidate shuffle (upstream `random.shuffle`), so pool selection
    /// spreads load across interchangeable refhosts instead of always taking the
    /// first in `refhosts.yml` order. Defaults to a real random shuffle; tests
    /// override it with the identity ([`ShuffleFn`]) for deterministic
    /// assertions.
    shuffle: ShuffleFn,
    /// Lazily-built, session-scoped outbound [`HttpClient`], cached with the
    /// [`VerifyPolicy`] it was built under.
    ///
    /// Every datasource-touching command historically built a fresh
    /// [`HttpClient`] per invocation; because `reqwest` fixes TLS and owns its
    /// connection pool at build time, that meant a cold pool per command (no
    /// cross-command keep-alive reuse). [`http_client`](Self::http_client) builds
    /// one on first use and hands out cheap `Arc`-backed clones thereafter,
    /// rebuilding only when the effective posture changes (e.g. a mid-session
    /// `config set ssl_verify`). Interior mutability so the `&Session` call sites
    /// (`export::build_http`) can lazily populate it; the lock is uncontended
    /// (one dispatch at a time). Perf bead `mtui-rs-0mop.13`.
    http_client: Mutex<Option<(VerifyPolicy, HttpClient)>>,
}

/// A candidate-order shuffle seam (upstream `random.shuffle`). Mutates the slot's
/// candidate list in place before the arbiter picks one.
pub type ShuffleFn = fn(&mut [String]);

/// The default [`ShuffleFn`]: a real random shuffle (upstream `random.shuffle`).
fn random_shuffle(candidates: &mut [String]) {
    use rand::seq::SliceRandom;
    candidates.shuffle(&mut rand::rng());
}

/// Render a refhosts [`Slot`](mtui_datasources::refhost::Slot) tuple as a stable
/// string key for [`TestReportBase::slot_candidates`]
/// (`product|version|arch|addon,addon`).
///
/// The tuple already sorts its addons, so this is a deterministic 1:1 encoding
/// used only as the map key that groups a slot's candidates for backup fallback.
fn slot_key(slot: &mtui_datasources::refhost::Slot) -> String {
    let (product, version, arch, addons) = slot;
    format!("{product}|{version}|{arch}|{}", addons.join(","))
}

/// The log levels `set_log_level` accepts (upstream `info`/`warning`/`error`/
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
    /// Parses the upstream level name, or `None` if unrecognised.
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

/// A callback the REPL installs to surface a desktop notification. Called with
/// the message and `true` for error-class toasts (upstream's
/// `stock_dialog-error` icon). Headless callers leave it unset.
pub type NotifySink = Box<dyn FnMut(&str, bool) + Send>;

impl Session {
    /// Builds a session for `config`, defaulting the display to stdout.
    ///
    /// `interactive` mirrors upstream: `true` for the REPL, `false` for MCP.
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
            http_client: Mutex::new(None),
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
            http_client: Mutex::new(None),
            #[cfg(test)]
            http_builds: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Builds a cheap per-call [`Session`] that **shares** this session's loaded
    /// reports and carries its own display sink.
    ///
    /// The headless MCP concurrency seam (`mtui-rs-f36r`, steps 4-5): a
    /// single-RRID tool call needs a `&mut Session` to dispatch, but holding the
    /// canonical session behind one mutex across dispatch serialises *all* calls.
    /// Instead the caller forks a per-call session — its
    /// [`TemplateRegistry::snapshot`] shares the same per-entry report locks, so a
    /// command acting on RRID `X` locks only `X`'s entry (letting a concurrent
    /// call on `Y` proceed), and the report content it mutates is visible to the
    /// canonical session (same `Arc<Mutex<..>>`).
    ///
    /// The fork clones the read-mostly `config` and copies `is_repl`/`shuffle`
    /// (all only mutated by `Scope::Single` commands, which run under the MCP
    /// exclusive gate against the *canonical* session, never concurrently with a
    /// forked per-RRID call), starts with a fresh empty `http_client` cache and
    /// no prompter/sinks (headless), and takes `display` as its own sink. It is
    /// therefore only sound to dispatch a **single-real-template**, non-mutating
    /// command through a fork; registry mutators and fan-out take the canonical
    /// session under the exclusive gate.
    #[must_use]
    pub fn fork_for_call(&self, display: CommandPromptDisplay) -> Self {
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
            http_client: Mutex::new(None),
            #[cfg(test)]
            http_builds: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The session-scoped outbound [`HttpClient`], built lazily and reused.
    ///
    /// The effective [`VerifyPolicy`] is resolved from `config.ssl_verify` (the
    /// same `resolve_verify(Default(true), Some(from_config(..)))` seam every
    /// datasource command used inline). The first call builds a client; later
    /// calls return a cheap `Arc`-backed clone of the cached one, so
    /// back-to-back commands reuse a single `reqwest` connection pool instead of
    /// churning a cold pool per invocation. If a mid-session `config set
    /// ssl_verify` changes the posture, the next call rebuilds under the new
    /// policy — the cache is keyed on the policy it was built with, so TLS
    /// behaviour never goes stale.
    ///
    /// # Errors
    ///
    /// Propagates [`HttpError`] when the client cannot be built (e.g. a
    /// configured CA bundle cannot be read); callers map it onto their own error
    /// type exactly as the previous inline `HttpClient::new` did.
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
    /// [`http_client`](Self::http_client) (perf-bead `mtui-rs-0mop.13` oracle).
    #[cfg(test)]
    fn http_builds(&self) -> usize {
        self.http_builds.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Makes `rrid` the active template *and* installs its per-call active
    /// handle ([`active_guard`](Self::active_guard)).
    ///
    /// The unified activation seam: it re-points the registry's active pointer
    /// and acquires the entry's lock so [`metadata`](Self::metadata) /
    /// [`targets`](Self::targets) read the right report. Any prior guard is
    /// dropped first (releasing that entry's lock) so re-activating the same
    /// entry never self-deadlocks. Returns `false` (leaving the guard cleared) if
    /// `rrid` is not loaded; passing an empty `rrid` clears the active pointer and
    /// guard so `metadata()` falls back to the null object (the empty-session
    /// state). Uses `try_lock_owned`: the outer session mutex serialises dispatch
    /// (steps 1-3), and the prior guard is dropped first, so the entry is always
    /// free here.
    pub fn activate(&mut self, rrid: &str) -> bool {
        // Drop any prior guard before acquiring the next so one Session never
        // holds two guards (and never blocks on an entry it already owns).
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
        self.active_guard.is_some()
    }

    /// Drops the per-call active handle *without* changing the active pointer.
    ///
    /// Used by teardown/probe paths that must lock entries directly
    /// (`quit`/`unload`/`load` replace, MCP `close`, and the fan-out
    /// `is_hostless` probe): they would self-deadlock on an entry this session's
    /// guard still holds. Unlike `activate("")` this leaves the registry's active
    /// RRID intact, so a survivor can still be promoted afterwards.
    pub fn release_active_guard(&mut self) {
        self.active_guard = None;
    }

    /// Re-installs the active handle for the registry's current active pointer.
    ///
    /// Used after a registry mutation (load) repoints `active` without going
    /// through [`activate`](Self::activate) — it drops any stale guard and locks
    /// the (possibly new) active entry, falling back to the null object when
    /// nothing is loaded.
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
    /// Lets guard-unaware callers (the hand-written MCP testreport tools) decide
    /// whether to read that report through [`metadata`](Self::metadata) (guard
    /// held) or by locking its entry handle directly (no guard).
    #[must_use]
    pub fn active_report_is_guarded(&self, rrid: &str) -> bool {
        self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid)
    }

    /// Whether the report loaded under `rrid` has no connected hosts.
    ///
    /// The guard-aware counterpart of
    /// [`TemplateRegistry::is_hostless`](crate::TemplateRegistry::is_hostless):
    /// when `rrid` is the currently-active template its entry is already locked
    /// by this session's [`active_guard`](Self::active_guard), so read through the
    /// guard rather than a (failing) `try_lock`. Other entries are locked
    /// directly.
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
    /// Guard-aware, like [`is_hostless`](Self::is_hostless): the active template's
    /// entry is read through the session guard, others are locked directly.
    #[must_use]
    pub(crate) fn template_row(&self, rrid: &str) -> Option<(usize, &'static str)> {
        if self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid) {
            let base = self.metadata().base();
            Some((base.targets.len(), base.workflow.as_str()))
        } else {
            self.templates.template_row(rrid)
        }
    }

    /// The active report (upstream `prompt.metadata`). Never `None` — falls back
    /// to the null object when nothing is loaded.
    #[must_use]
    pub fn metadata(&self) -> &(dyn TestReport + Send + Sync) {
        match &self.active_guard {
            Some(g) => &***g,
            None => &*self.null,
        }
    }

    /// Mutably borrows the active report (upstream `prompt.metadata`, mutated).
    ///
    /// The mutable counterpart of [`metadata`](Self::metadata). The
    /// `reload_openqa` / `set_workflow` commands populate the report's openQA
    /// holder ([`TestReport::openqa_mut`]) through it; never `None` (falls back to
    /// the null object when nothing is loaded).
    pub(crate) fn metadata_mut(&mut self) -> &mut (dyn TestReport + Send + Sync) {
        match &mut self.active_guard {
            Some(g) => (**g).as_mut(),
            None => self.null.as_mut(),
        }
    }

    /// Sets the active report's [`Workflow`] mode (upstream
    /// `metadata.workflow = …`).
    ///
    /// The one mutable window onto the active report's workflow. `add_host`
    /// (and later `set_workflow`) uses it to move an automatic session to
    /// manual. Upstream additionally calls `prompt.set_prompt()` to refresh the
    /// REPL prompt string; that prompt refresh is a Phase-6 REPL concern, so the
    /// command only mutates the report here.
    pub(crate) fn set_workflow(&mut self, workflow: Workflow) {
        self.metadata_mut().base_mut().workflow = workflow;
    }

    /// The active report's connected targets (upstream `prompt.targets`).
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.metadata().base().targets
    }

    /// Mutably borrows the active report's connected targets.
    ///
    /// The mutable counterpart of [`targets`](Self::targets); command bodies
    /// that fan a command out across hosts (`run`, `reboot`, `set_repo`) need
    /// `&mut HostsGroup`.
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.metadata_mut().base_mut().targets
    }

    /// Moves the active report's targets out, leaving an empty group in place.
    ///
    /// The counterpart to [`restore_targets`](Self::restore_targets). The
    /// report's `perform_*` methods take `&self` **and** `&mut HostsGroup`;
    /// because the targets live inside the active report, a single
    /// `&mut Box<dyn TestReport>` cannot hand out both borrows at once. Taking
    /// the group out by value breaks that tie: the caller then holds an owned
    /// `HostsGroup` (no borrow of `self`) and can freely re-borrow the report via
    /// [`metadata`](Self::metadata) to drive `perform_*`, restoring the group
    /// afterwards.
    ///
    /// Mirrors upstream, where a command reads `self.metadata` and `self.targets`
    /// as two views of the same active report.
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
    /// The lossless replacement for `take_targets()` + `HostsGroup::select` in
    /// the `perform_*` / `set_repo` drivers. A `-t` subset operation must run over
    /// only the selected hosts, yet the unselected hosts must survive in the live
    /// report — upstream gets this for free because its child group shares
    /// `Target` references with the parent dict, but a Rust `Target` owns its
    /// connection and cannot be shared. This hands back both halves so the driver
    /// can drive the op over `selected`, then hand both back to
    /// [`restore_split_targets`](Self::restore_split_targets), which merges the
    /// remainder back in.
    ///
    /// `hosts` is the parsed `-t` value: `None` (or `-t all`, which callers pass
    /// as `None`) selects every enabled host with an empty remainder; `Some` names
    /// exactly those hosts and keeps the rest in the remainder. Selection is
    /// `enabled`-filtered (disabled hosts land in the remainder, never dropped).
    ///
    /// # Errors
    ///
    /// [`mtui_hosts::HostError::NotConnected`] when a named `-t` host is not a
    /// member of the active report's group.
    ///
    /// On error the group is left empty in the report (the taken group is
    /// consumed by the failed split); callers surface the error immediately, so
    /// no host is observable in that window.
    pub(crate) fn split_targets(
        &mut self,
        hosts: Option<&[String]>,
    ) -> mtui_hosts::Result<(HostsGroup, HostsGroup)> {
        self.take_targets().select_split(hosts, true)
    }

    /// Merges the untouched `remainder` back into the operated `selected` group
    /// and restores it as the active report's targets.
    ///
    /// The counterpart to [`split_targets`](Self::split_targets): recombining the
    /// two halves preserves the hosts a `-t` subset operation did not touch.
    pub(crate) fn restore_split_targets(
        &mut self,
        mut selected: HostsGroup,
        remainder: HostsGroup,
    ) {
        selected.merge(remainder);
        self.restore_targets(selected);
    }

    /// Loads a template into the registry and, when requested, connects its
    /// reference hosts (upstream `prompt.load_update`).
    ///
    /// Mirrors upstream `CommandPrompt.load_update`:
    ///
    /// 1. [`make_testreport`] checks out and reads the report (or returns a null
    ///    report on failure, which [`TemplateRegistry::add`] silently ignores).
    /// 2. The report is added to the registry and — when it carries a real RRID —
    ///    made active. Re-loading an already-loaded RRID replaces its stored
    ///    report and makes it active; sibling templates are untouched.
    /// 3. If the report asked for autoconnect ([`TestReportBase::autoconnect_pending`],
    ///    set by `make_testreport` for `-a` with `autoconnect`), its reference
    ///    hosts are connected. The connect is driven **here** (the composition
    ///    root) rather than inside `mtui-testreport`, so that crate never depends
    ///    on `mtui-hosts`/`mtui-datasources` — no crate cycle.
    ///
    /// The connect resolves hosts from two sources, matching upstream
    /// `TestReport.autoconnect`: the template's own `reference host:` lines
    /// (already parsed into `hostnames`) plus one host per matching slot resolved
    /// from each testplatform through the refhosts inventory. Every connect is
    /// best-effort: an unreachable host is logged and skipped so one dead host
    /// never aborts the load.
    ///
    /// Returns the loaded report's RRID (empty when the load failed and the null
    /// report was substituted).
    ///
    /// A thin wrapper over [`load_update_reported`](Self::load_update_reported)
    /// that discards the failure reason, for callers that only branch on
    /// success/failure (REPL startup, `regenerate`).
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
    /// Returns `(rrid, load_error)`: on success the RRID and `None`; on failure
    /// an empty RRID and `Some(reason)` — the diagnostic
    /// [`make_testreport`](make_testreport) stashed on the substituted null
    /// report (svn checkout / gitea / hash / read failure). Lets `load_template`
    /// surface the real cause to the operator (and, via MCP, the LLM) instead of
    /// a bare "could not load".
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
        // Capture the failure reason before the report is moved into the registry
        // (the null sentinel is dropped by `add_or_replace`; a real report has
        // `None` here).
        let load_error = if rrid.is_empty() {
            report.base().load_error.clone()
        } else {
            None
        };

        // Release any held active handle before the (possibly same-RRID) replace:
        // `add_or_replace` tears the old report down by locking its entry, which
        // would self-deadlock against a guard this session still holds on it
        // (e.g. `regenerate` reloading the active template). The guard is
        // re-installed for the freshly-loaded template below.
        self.active_guard = None;

        // `add_or_replace` ignores the empty-RRID null sentinel; a real report
        // becomes active (re-load tears the previous same-RRID report down —
        // releasing its arbiter claim + remote pool/operation locks and closing
        // its hosts — before storing the new one, then re-activates). Teardown
        // failures on the replaced report are best-effort logged.
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
        // Re-install the per-call active handle for whatever is now active: the
        // freshly-loaded template on success, or the *unchanged* prior active
        // when the load failed (empty rrid → `add_or_replace` ignored the null
        // sentinel and the pointer never moved). This restores the guard released
        // above so the autoconnect below (and the caller after this returns) read
        // through `metadata()`/`targets_mut()`.
        self.refresh_active_guard();

        if pending && !rrid.is_empty() {
            self.autoconnect_active(&rrid).await;
        }
        (rrid, load_error)
    }

    /// Connects the active report's reference hosts (the deferred half of
    /// [`load_update`](Self::load_update)).
    ///
    /// Computes the wanted host list via [`autoconnect_hosts`](Self::autoconnect_hosts)
    /// — the template's parsed `reference host:` names plus one host per matching
    /// slot resolved from each testplatform — then builds and connects a
    /// [`Target`] for each, stamping the report's RRID as the pool-claim
    /// ownership identity. Connect failures are logged and the host dropped
    /// (best-effort, upstream `connect_targets`).
    ///
    /// The offline host-selection is factored into the pure, unit-tested
    /// [`autoconnect_hosts`](Self::autoconnect_hosts); this thin connect loop
    /// builds real [`Target`]s and is exercised by the gated sshd integration
    /// path (the same seam `list_refhosts --free` uses for its live probe).
    async fn autoconnect_active(&mut self, rrid: &str) {
        // Snapshot everything needed from the active report *synchronously* (no
        // `&Session` may cross the resolver await — `Session` is not `Sync`, so a
        // borrow held across the await would make this future non-`Send`, which
        // the `Command::call` trait requires).
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
        // slot) when the arbiter + owner are wired (upstream
        // `_pool_selection_active`); this is the composition-root default.
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
    /// The shared connect loop behind [`autoconnect_active`](Self::autoconnect_active)
    /// and the `add_host` command. Each target is stamped with `rrid` (the
    /// pool-claim ownership identity) before connecting, mirroring upstream's
    /// `Target(..., _rrid=...)`. A target built via [`Target::new`] is
    /// unconnected, so [`Target::connect`] performs the live SSH connect; a
    /// caller that pre-builds connected targets (tests over a mock connection)
    /// sees `connect` short-circuit as a no-op.
    ///
    /// After a successful connect, a freshly added host is autolocked with the
    /// active report's `lock_comment` when a PI assignment is in progress
    /// (upstream `_autolock_new_target`, called from both `add_target` and
    /// `connect_targets`): a host already locked by another owner is left as-is
    /// ([`HostError::TargetLocked`] suppressed), and a failed autolock never
    /// drops an otherwise-good host.
    ///
    /// Each connected host is also checked for product drift against its
    /// `refhosts.yml` row ([`verify_target_products`](Self::verify_target_products),
    /// upstream `_verify_target_products`): mismatches are surfaced to the user,
    /// recorded in the report's `product_warnings`, and WARN-logged, but never
    /// drop the host. The refhosts inventory is built once for the batch; if it
    /// is unavailable the check is silently skipped (upstream store `None`).
    async fn connect_and_add_hosts(&mut self, hosts: Vec<String>, rrid: &str) {
        let config = self.config.clone();
        // Snapshot the active report's PI-lock comment before the connect loop:
        // a `base()` borrow held across the connect `.await` would make this
        // future non-`Send` (the `Command::call` bound), exactly the constraint
        // the `config`/`timeout_prompt` snapshots below exist for. Empty when no
        // PI assignment is active (upstream `lock_comment == ""`).
        let lock_comment = self.metadata().base().lock_comment.clone();
        // Snapshot the active report's package metadata (`product -> { name ->
        // required-version }`) before the `targets_mut()` borrow below. Cloning
        // it up front keeps the connect future `Send` (a `base()` borrow held
        // across the connect `.await` would not be) and is the port of upstream
        // `Target._parse_packages`, which seeds each host's tracked packages —
        // with their required versions — right after connect(). Empty when no
        // report (or a report with no packages) is loaded, in which case seeding
        // is a no-op, matching upstream's empty `self.packages` pre-load.
        let package_meta = self.metadata().base().packages.clone();
        // Snapshot the command-timeout prompt (a `Clone`-able closure) before the
        // connect loop: a `&Session`/`&Prompter` borrow held across the connect
        // `.await` would make this future non-`Send`, which `Command::call`
        // requires. `None` (headless / `mtui-mcp`) leaves the timeout an
        // immediate abort (upstream `prompter=None`).
        let timeout_prompt = self.prompter.as_ref().map(Prompter::as_timeout_prompt);
        let prompter = self.prompter.clone();
        // Build the refhosts inventory once for the batch (upstream's memoized
        // `_get_refhosts_store`). `None` on any failure disables the drift check
        // for every host — best-effort, never fatal. Built before the
        // `targets_mut()` borrow so this await does not straddle it.
        let store = Self::build_refhosts_store(&config).await;
        // Drift results collected during the loop (a `base_mut()`/`self.display`
        // borrow held across the connect `.await` would make the future
        // non-`Send`): `Some(lines)` records drift, `None` clears any stale entry
        // for a host that now matches / is absent. Applied after the loop.
        // Connect every host concurrently: each host's connect + autolock +
        // package-seed + drift-verify runs as an independent future and the
        // whole batch is driven together, so attaching N hosts costs one slow
        // handshake, not the sum of all of them. The futures share `&config` /
        // `&store` / `&package_meta` (all plain data), so no per-host clone of
        // those is needed; each produces its own owned `Target` + drift entry.
        // Snapshot the pool claims so each host knows whether to take the remote
        // pool lock (upstream `host in self._pool_claims`) vs. the normal
        // autolock. Empty on the legacy `add_host --target` path.
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
        // Bound the connect fan-out to `[connection] max_parallel` so attaching a
        // large fleet caps peak concurrent SSH handshakes/sockets/tasks rather
        // than opening one per host at once. The per-host connect futures borrow
        // `&config`/`&store` (a spawn-free in-place fan-out), which does not fit
        // `buffer_unordered`'s stream bounds; chunk the iterator into batches of
        // `bound` and `join_all` each batch instead. This caps peak concurrency
        // at `bound` while keeping the exact per-batch `join_all` semantics.
        // Completion/order within a batch is irrelevant: results fold into a
        // sorted `BTreeMap` group below.
        let bound = (config.max_parallel as usize).max(1);
        let connect_futs: Vec<_> = connect_futs.collect();
        let mut connected = Vec::with_capacity(connect_futs.len());
        let mut iter = connect_futs.into_iter().peekable();
        while iter.peek().is_some() {
            let batch: Vec<_> = iter.by_ref().take(bound).collect();
            connected.extend(futures::future::join_all(batch).await);
        }

        let mut drift: Vec<(String, Option<Vec<String>>)> = Vec::new();
        // Track which hosts connected so the pool-backup step (below) can tell
        // which slots still need a live host.
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let targets = self.targets_mut();
        // Ensure the (possibly freshly-loaded) active group carries the prompter
        // so its serial-barrier Enter prompt fires; a group built by a later
        // `load_update` would otherwise start without it.
        if let Some(prompter) = prompter {
            targets.set_prompter(prompter);
        }
        // Fold the successful connects into the group (sorted-keyed BTreeMap, so
        // the concurrent completion order is irrelevant) and collect drift.
        for (target, drift_entry) in connected.into_iter().flatten() {
            live.insert(target.hostname().to_owned());
            targets.add(target);
            drift.push(drift_entry);
        }

        // Backup-refhost fallback (upstream `_connect_pool_backups`): for any
        // pool slot whose chosen host failed to connect, sequentially try the
        // remaining free siblings until one connects or the slot is exhausted.
        let backup_drift = self
            .connect_pool_backups(&config, rrid, &hosts, &live, timeout_prompt.clone())
            .await;
        drift.extend(backup_drift);

        // Surface + persist drift now that the `targets_mut()` borrow is released.
        self.apply_product_warnings(drift);
    }

    /// Connects a single host, autolocks + package-seeds + drift-verifies it, and
    /// returns the live [`Target`] plus its drift entry, or `None` on failure.
    ///
    /// Extracted from [`connect_and_add_hosts`](Self::connect_and_add_hosts) so
    /// both the concurrent initial batch and the sequential backup-refhost
    /// fallback ([`connect_pool_backups`](Self::connect_pool_backups)) share one
    /// connect path. All inputs are borrowed/owned plain data so the returned
    /// future stays `Send` (the `Command::call` bound).
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
        let mut target = Target::new(
            config,
            host.clone(),
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        target.set_rrid(rrid.to_owned());
        // Wire the interactive command-timeout prompt before connecting
        // so `Target::connect` applies it to the transport (REPL only).
        if let Some(tp) = timeout_prompt.as_ref() {
            target.set_timeout_prompt(tp.clone());
        }
        match target.connect().await {
            Ok(()) => {
                if is_pool_claim {
                    // Take the remote pool lock (upstream `try_claim` in
                    // `connect_target`): the `mtui pool <RRID> [<RRID>]` stamp.
                    // Losing the remote race means another process holds this
                    // host — drop it so a sibling in the slot can be tried (the
                    // in-process claim is released by `connect_pool_backups`).
                    let comment = format!("mtui pool {rrid} [{rrid}]");
                    match target.pool_claim(&comment).await {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(host = %host, "claimed in-process but busy remotely; skipping");
                            return None;
                        }
                        Err(e) => {
                            warn!(host = %host, error = %e, "pool claim failed remotely; skipping");
                            return None;
                        }
                    }
                } else {
                    Self::autolock_target(&mut target, lock_comment).await;
                }
                // Seed the host's tracked packages with their metadata
                // `required` versions (upstream `_parse_packages`), keyed by the
                // just-parsed base product version, then query current versions
                // so `list_packages` / `package_check` / `downgrade` all see a
                // populated list. `connect()` already parsed the system, so
                // `get_base().version` is authoritative here.
                let base_version = target.system().get_base().version.clone();
                let seeded =
                    mtui_testreport::testreport::packages_for_map(package_meta, &base_version);
                if !seeded.is_empty() {
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
    /// (upstream `_connect_pool_backups`, RFC §5.7 backup-refhost).
    ///
    /// For each slot in the active report's `slot_candidates` whose chosen host
    /// is not among the just-connected `live` hosts: drop the dead claim(s),
    /// then sequentially `acquire_any` the next free sibling and connect it,
    /// until one succeeds or the slot's candidates are exhausted. Any host that
    /// connects is added to the active group and its drift entry returned.
    ///
    /// A no-op when pool selection is inactive (`arbiter`/`owner` unset) or no
    /// slots are recorded. Best-effort: connect failures release the in-process
    /// claim and move to the next sibling.
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

        for (slot, candidates) in slot_candidates {
            // Slot already has a live connection? Nothing to do.
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
                        // Release the claim so the next candidate is free to try.
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
    /// Ports upstream `_verify_target_products`: looks the host up in `store`
    /// ([`compare`] against its [`Host`](mtui_types::Product) row) and returns
    /// `Some(lines)` when [`ProductDiff`](mtui_datasources::ProductDiff) reports
    /// drift (base/arch/addon/dangling-symlink; the `qa` addon is always
    /// ignored, handled inside `compare`). Returns `None` — meaning "no drift,
    /// clear any stale entry" — when the store is unavailable, the host is absent
    /// from `refhosts.yml`, or the products match. Best-effort: never fails a
    /// connect; the host is kept regardless.
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

    /// Applies collected product-drift results to the active report and surfaces
    /// them to the user (upstream stores `product_warnings` and logs each line).
    ///
    /// `Some(lines)` records drift under the hostname and prints a yellow warning
    /// block so the mismatch is visible while adding the host; `None` clears any
    /// stale entry for a host that now matches or is absent from `refhosts.yml`.
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
    /// Ports upstream `_autolock_new_target`: a no-op when `lock_comment` is empty
    /// (no PI assignment active). A host already locked by another owner is left
    /// as-is ([`HostError::TargetLocked`] suppressed, logged at debug, mirroring
    /// `Target::unlock`); any other lock error is logged at `warn` but never
    /// propagated, so a failed autolock never drops an otherwise-good host from
    /// the batch (best-effort, matching upstream `suppress(TargetLockedError)`).
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
    /// The `add_host`-without-`-t` path (upstream `for tp in
    /// metadata.testplatforms: refhosts_from_tp(tp)` then `connect_targets()`):
    /// each testplatform contributes one candidate host per matching slot,
    /// deduplicated against the hosts already in the group, then connected.
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
        // Same pool-selection path as autoconnect: one host per requested slot
        // (arbiter chosen) when wired, else the legacy connect-every-candidate
        // path. `ref_hosts` is empty here — `add_host` (no `-t`) draws purely
        // from the testplatforms.
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
    /// The `add_host`-with-`-t` path (upstream `add_target(hostname)` per host):
    /// each host is stamped with the active report's RRID and connected.
    ///
    /// A host already in the active group is warned about and skipped (upstream
    /// `add_target`: `"already connected to <h>, skipping."` then early return),
    /// matching the silent dedup the no-`-t` path already does in
    /// [`add_testplatform_hosts`](Self::add_testplatform_hosts). The membership
    /// snapshot is taken before any `.await` so the connect future stays `Send`.
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
    /// The shared store-builder behind [`resolve_testplatform_hosts`] (host
    /// selection) and [`verify_target_products`](Self::verify_target_products)
    /// (post-connect product-drift check) — the same on-demand pattern
    /// `list_refhosts`/`add_host` use, with no cached Session state. A `None`
    /// result degrades both callers to a no-op (upstream `except
    /// RefhostsResolveFailedError: return` / `_get_refhosts_store() is None`).
    ///
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

    /// Resolves one candidate host per matching slot from the given
    /// testplatforms (the refhosts-from-testplatform half of autoconnect).
    ///
    /// Builds the refhosts factory on demand from `config` (the same pattern
    /// `list_refhosts`/`add_host` use — no cached Session state), resolves the
    /// inventory, and for each testplatform searches for matching host names. A
    /// resolver failure degrades to an empty result (upstream `except
    /// RefhostsResolveFailedError: return`), so autoconnect still connects the
    /// template's own reference hosts.
    ///
    /// Takes owned/borrowed plain data (not `&Session`) so the caller's connect
    /// future stays `Send` across this await.
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

    /// Pick one distinct free host per test-target slot via the arbiter
    /// (upstream `_pool_select_from_tp`, run per testplatform).
    ///
    /// For each testplatform: [`search_pool_by_query`](Refhosts::search_pool_by_query)
    /// groups candidates by their *requested* slot (product+version+arch+requested
    /// addons), so hosts interchangeable for the update collapse to one slot. Each
    /// slot's candidates are shuffled (via the [`ShuffleFn`] seam, upstream
    /// `random.shuffle`) and recorded so a failed connect can fall
    /// back to a sibling; a slot this owner already holds a host for (across
    /// testplatforms) is skipped; otherwise one free host is claimed through the
    /// arbiter (waiting up to `[lock] wait` seconds when all candidates are busy).
    ///
    /// Returns `(chosen_hosts, slot_candidates)`: the claimed hosts (this batch's
    /// `pool_claims`) and the per-slot ordered candidate lists (keyed by the slot
    /// rendered as a stable string, matching [`TestReportBase::slot_candidates`]).
    /// The caller writes both onto the active report before connecting.
    ///
    /// Static (owned/borrowed plain data, `&'static` arbiter) so the caller's
    /// connect future stays `Send`.
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
            // Group candidate host names by slot (preserving first-seen slot
            // order for stable iteration). IndexMap keeps insertion order, so the
            // grouping is O(pairs) instead of the previous O(pairs × slots) scan.
            let mut by_slot: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
            for (host, slot) in pairs {
                by_slot.entry(slot_key(&slot)).or_default().push(host.name);
            }

            for (slot, mut candidates) in by_slot {
                // Spread load across interchangeable hosts (upstream shuffle),
                // then remember the order for backup-refhost fallback.
                shuffle(&mut candidates);
                slot_candidates.insert(slot.clone(), candidates.clone());

                // Skip slots we already hold a host for (across testplatforms).
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
    /// The shared selection step behind [`autoconnect_active`](Self::autoconnect_active)
    /// and [`add_testplatform_hosts`](Self::add_testplatform_hosts). When the
    /// arbiter + owner are wired (`_pool_selection_active`), each testplatform
    /// contributes one arbiter-chosen host per requested slot (via
    /// [`pool_select`](Self::pool_select)) and the chosen hosts are recorded as
    /// `pool_claims` so [`connect_and_add_hosts`](Self::connect_and_add_hosts)
    /// connects only them (with sibling backup fallback). Without the arbiter it
    /// degrades to the legacy `search()` path (connect every candidate).
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
            // Pool-selection path (upstream `_pool_select_from_tp`).
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
                    // Record claims + candidates on the active report so
                    // connect_and_add_hosts connects only the claims (and can
                    // fall back to siblings) and quit can release them.
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
            // Legacy path (no arbiter/owner): connect every search() match.
            _ => Self::resolve_testplatform_hosts(config, &testplatforms).await,
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
    /// Set by the `quit` command; read by the Phase-6 REPL via
    /// [`should_exit`](Self::should_exit).
    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    /// Whether the `quit` command has asked the REPL loop to exit.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Installs the callback `set_log_level` uses to apply a runtime level change.
    ///
    /// The Phase-6 REPL wires this to a `tracing_subscriber::reload` handle so
    /// `set_log_level debug` takes effect immediately; headless callers leave it
    /// unset.
    pub fn set_log_level_sink(&mut self, sink: LogLevelSink) {
        self.log_level_sink = Some(sink);
    }

    /// Applies `level` through the installed sink, if any (upstream
    /// `prompt.log.setLevel`).
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

    /// Installs the callback [`notify_user`](Self::notify_user) uses to surface a
    /// desktop notification.
    ///
    /// The Phase-6 REPL wires this to `mtui-cli`'s `notification::notify_user`;
    /// headless callers (`mtui-mcp`) and tests leave it unset, making
    /// notifications a silent no-op.
    pub fn set_notify_sink(&mut self, sink: NotifySink) {
        self.notify_sink = Some(sink);
    }

    /// Surfaces a best-effort desktop notification through the installed sink, if
    /// any (upstream `prompt.notify_user`).
    ///
    /// `error` selects the error-class toast (upstream's `stock_dialog-error`).
    /// Returns `true` when a sink was present and invoked; `false` when none is
    /// installed (headless/tests).
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
    /// [`Prompter::stdin`](mtui_hosts::Prompter::stdin)-backed prompter here for
    /// the REPL; `mtui-mcp` leaves it unset. Also pushes the prompter onto the
    /// active report's [`HostsGroup`] so any already-connected hosts (and the
    /// serial-barrier Enter prompt) pick it up immediately; freshly-connected
    /// hosts inherit the derived command-timeout prompt via
    /// [`connect_and_add_hosts`](Self::connect_and_add_hosts).
    pub fn set_prompter(&mut self, prompter: Prompter) {
        // Push onto the active report's group first (already-connected hosts +
        // the serial-barrier prompt), then retain a clone for future connects.
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

    /// Perf-bead `mtui-rs-0mop.13` oracle: repeated `http_client()` calls under
    /// a stable `ssl_verify` reuse one built client (a cheap clone), and a
    /// mid-session posture change rebuilds exactly once. Guards against a
    /// regression to per-command client construction.
    #[test]
    fn http_client_is_reused_and_rebuilt_on_posture_change() {
        use mtui_config::SslVerify;

        let mut s = Session::new(config(), true);
        assert_eq!(s.http_builds(), 0, "no client until first use");

        // First use builds; three more calls with unchanged config reuse it.
        let c0 = s.http_client().expect("client builds");
        for _ in 0..3 {
            let _ = s.http_client().expect("cached clone");
        }
        assert_eq!(s.http_builds(), 1, "one build shared across four calls");
        drop(c0);

        // Flipping ssl_verify changes the resolved policy -> exactly one rebuild.
        s.config.ssl_verify = SslVerify::Disabled;
        let _ = s.http_client().expect("rebuild under new posture");
        assert_eq!(s.http_builds(), 2, "posture change rebuilds once");
        // ...and the new posture is then itself reused, not rebuilt again.
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

    #[test]
    fn prompter_is_none_until_installed_then_some() {
        let mut s = Session::new(config(), true);
        assert!(s.prompter().is_none());
        // A no-op prompter (no stdin) installed by the composition root.
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
        // No sink installed → returns false, no panic.
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
        // Mirrors the production `mtui-cli::main` seam: `Session::new` builds a
        // stdout display defaulting to `Never`, then `--color` is applied via
        // `set_color`. Regression guard for Gap 0 (colors never appeared because
        // the resolved mode was never handed to the display).
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

    // --- Sub-bead B: load_update + autoconnect host resolution -------------

    use mtui_hosts::MockConnection;
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;

    const REFHOSTS_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mtui-datasources/tests/fixtures/refhosts.yml"
    );

    /// A config whose refhosts resolver is the offline file-backed `path`
    /// resolver pointed at the ported fixture (no network).
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

    /// Reconstructs the legacy (non-pool) autoconnect host set from the active
    /// report: reference hosts merged with the `search()`-resolved testplatform
    /// hosts, minus the already-connected ones — the fallback path
    /// [`Session::resolve_and_record_pool`] takes when the arbiter is unwired.
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

    /// `autoconnect_hosts` combines the template's reference hosts with the
    /// hosts resolved from its testplatforms (offline `path` resolver), sorted
    /// and deduplicated.
    #[tokio::test]
    async fn autoconnect_hosts_merges_reference_and_testplatform_hosts() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        // A testplatform matching the sles 15.5 x86_64 hosts in the fixture
        // (fixture minor is the numeric `5`, so the query must use `minor=5`).
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-a.example.com"],
            &["base=sles(major=15,minor=5);arch=[x86_64]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;

        // The explicit reference host is always present.
        assert!(hosts.contains(&"ref-a.example.com".to_owned()));
        // The testplatform resolved at least one fixture host (sles 15-SP5 x86_64).
        assert!(
            hosts.iter().any(|h| h.contains("x86")),
            "expected a resolved x86 refhost, got: {hosts:?}"
        );
        // Deduplicated: no host appears twice.
        let mut sorted = hosts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), hosts.len(), "hosts must be deduplicated");
    }

    /// With no testplatforms, `autoconnect_hosts` is exactly the reference-host
    /// set (no resolver call needed).
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

    /// `load_update` for a kernel update loads the on-disk template, activates
    /// it, and does **not** autoconnect (so no live-host access on load).
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
        // Kernel does not autoconnect: no targets were connected.
        assert!(s.targets().is_empty());
    }

    /// `load_update` for an unloadable RRID (no template, offline `svn`) falls
    /// back to the null report: nothing is registered, empty RRID returned.
    #[tokio::test]
    async fn load_update_missing_report_returns_empty_and_registers_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        // Force the internal `svn co` to fail fast offline.
        config.svn_path = format!("file://{}/no-such-repo", tmp.path().display());
        let mut s = Session::new(config, false);

        let update = UpdateID::parse("SUSE:Maintenance:1:1").unwrap();
        let (loaded, reason) = s
            .load_update_reported(&update, true, UpdateKind::Auto)
            .await;

        assert_eq!(loaded, "");
        assert!(s.templates.is_empty());
        // The failure reason is threaded back for the caller to surface.
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

    /// `add_named_hosts` drives the connect loop; unreachable hosts fail their
    /// live connect and are skipped rather than added.
    #[tokio::test]
    async fn add_named_hosts_skips_unconnectable() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        s.add_named_hosts(vec!["unreachable.invalid".to_owned()])
            .await;
        assert!(s.targets().is_empty());
    }

    /// A host already in the active group is skipped, not re-added: `add_named_hosts`
    /// warns and drops it before the connect loop (upstream `add_target`'s
    /// `"already connected … skipping"` early return). The group size is unchanged.
    #[tokio::test]
    async fn add_named_hosts_skips_already_connected() {
        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        // Pre-seed the active group with a connected mock target.
        s.targets_mut().add(mock_target("refhost.example"));
        assert_eq!(s.targets().len(), 1);
        assert!(s.targets().contains("refhost.example"));

        // Re-adding the same name must not connect a second target.
        s.add_named_hosts(vec!["refhost.example".to_owned()]).await;

        assert_eq!(
            s.targets().len(),
            1,
            "already-connected host must not be re-added"
        );
    }

    /// `add_testplatform_hosts` resolves the active report's testplatforms via
    /// the offline `path` resolver, then connects them; unreachable fixture
    /// hosts are skipped, but the resolution path is exercised without panicking.
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
        // Fixture hosts are not reachable, so none are added.
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
    /// `split_targets` must propagate the session's `is_repl` mode to the taken
    /// group and both split halves, so the fan-out spinner/prompt seam is not
    /// silently suppressed on the perform_* path. The session is the single
    /// source of truth; the empty replacement group it leaves behind also carries
    /// the flag (a later `load_update` re-sets it at load time).
    #[tokio::test]
    async fn take_and_split_targets_propagate_session_is_repl() {
        let mut s = Session::new(config_with_path_refhosts(), true);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        // Simulate the load-time reconcile that `make_testreport` performs, then
        // add a connected host into the (now interactive) report group.
        s.targets_mut().set_is_repl(true);
        s.targets_mut().add(mock_target("refhost.example"));

        let taken = s.take_targets();
        assert!(
            taken.is_repl(),
            "take_targets must hand back an is_repl=true group"
        );
        s.restore_targets(taken);

        let (selected, remainder) = s.split_targets(None).expect("split");
        assert!(selected.is_repl(), "split selected half must be is_repl");
        assert!(remainder.is_repl(), "split remainder half must be is_repl");
    }

    /// Builds a mock-backed, already-connected [`Target`] — the test seam the
    /// connect loop reaches once `Target::connect` short-circuits.
    fn mock_target(host: &str) -> Target {
        Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(MockConnection::new(host)),
        )
    }

    /// `autolock_target` locks a freshly connected host with the PI comment when
    /// a `lock_comment` is active (upstream `_autolock_new_target`).
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

    /// A host already locked by another owner is left as-is: the foreign
    /// [`HostError::TargetLocked`] is suppressed and `autolock_target` returns
    /// without error (upstream `suppress(TargetLockedError)`).
    #[tokio::test]
    async fn autolock_target_suppresses_foreign_lock() {
        // Pre-seed a fresh foreign lock file so the mock's lock read sees another
        // owner (huge future pid, distinct user) and refuses to relock.
        let conn = MockConnection::new("refhost.example").with_file(
            "/var/lock/mtui.lock",
            format!("{}:someone-else:2147483647", i64::MAX),
        );
        let mut t = Target::with_connection(
            "refhost.example",
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(conn),
        );
        // Must not panic / propagate: the foreign lock is suppressed.
        Session::autolock_target(&mut t, "mtui pool SUSE:Maintenance:1:1 alice").await;
    }

    // --- product-drift verification (upstream `_verify_target_products`) -----

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

    /// A host whose detected products match its `refhosts.yml` row yields no
    /// warnings (`None` clears any stale entry).
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

    /// The `qa` addon is always ignored: a host carrying only an extra `qa`
    /// addon over its row is still a match (drift check inside `compare`).
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
        // Pre-seed a stale entry that a later match should clear.
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

    // --- pool selection (host over-selection fix, mtui-rs-4eq) --------------

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

    /// A leaked, empty process-local arbiter for tests (gives the `&'static`
    /// the pool API expects without touching the shared global singleton).
    fn test_arbiter() -> &'static HostArbiter {
        Box::leak(Box::new(HostArbiter::new()))
    }

    /// Identity shuffle so pool selection is deterministic in tests.
    fn no_shuffle(_c: &mut [String]) {}

    /// `pool_select` collapses interchangeable hosts (same requested slot) to a
    /// single arbiter-chosen host, and keeps distinct arches as distinct slots.
    #[tokio::test]
    async fn pool_select_one_host_per_requested_slot() {
        // Two x86_64 SP5 hosts (interchangeable) + one ppc64le SP5 host.
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

        // One host per slot: the two x86 hosts collapse to one, ppc adds one.
        assert_eq!(
            chosen.len(),
            2,
            "expected one host per slot, got {chosen:?}"
        );
        assert_eq!(slot_candidates.len(), 2, "two distinct slots recorded");
        // The x86 slot recorded both interchangeable candidates for backup.
        let x86_slot = slot_candidates
            .values()
            .find(|c| c.contains(&"x86-a".to_owned()) || c.contains(&"x86-b".to_owned()))
            .expect("x86 slot present");
        assert_eq!(
            x86_slot.len(),
            2,
            "both x86 hosts kept as backup candidates"
        );
        // Deterministic shuffle → first candidate chosen per slot.
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

        // Slot already owned → nothing newly claimed.
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

    /// The IndexMap slot grouping (0mop.12 lever 1) preserves **first-seen slot
    /// order**: hosts are claimed in the order their slots first appear in the
    /// `search_pool_by_query` output (arch fan-out order), exactly as the prior
    /// `Vec`-of-pairs grouping did. Guards against an accidental switch to an
    /// unordered map.
    #[tokio::test]
    async fn pool_select_preserves_first_seen_slot_order() {
        // Interleave arches so the first-seen slot order is ppc → x86 (not
        // alphabetical): the store rows and the arch list both lead with ppc.
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

        // One host per slot, in first-seen (ppc-then-x86) slot order.
        assert_eq!(
            chosen,
            vec!["ppc-a".to_owned(), "x86-a".to_owned()],
            "chosen hosts must follow first-seen slot order"
        );
        assert_eq!(slot_candidates.len(), 2, "two distinct arch slots");
    }

    /// `fork_for_call` shares the canonical session's loaded reports (same entry
    /// locks) while carrying its own display, so a per-RRID command dispatched on
    /// a fork mutates the *shared* report content visible to the canonical
    /// session (`mtui-rs-f36r`, steps 4-5).
    #[test]
    fn fork_for_call_shares_reports_with_own_display() {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let mut s = Session::new(config_with_path_refhosts(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);

        // The canonical session must not hold a guard on the entry a fork will
        // lock (the MCP `run_command` exclusive path releases it after each call,
        // exactly so a later fork can activate the shared entry).
        s.release_active_guard();

        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let mut fork = s.fork_for_call(display);
        // Own (distinct) display.
        assert_eq!(fork.display.color(), ColorMode::Always);
        // Shared entry: activating the same RRID on the fork locks the shared
        // report and mutating it is observable through the canonical session's
        // registry handle.
        assert!(fork.activate("SUSE:Maintenance:1:1"));
        fork.set_workflow(Workflow::Auto);
        // Drop the fork's guard so the canonical read can lock the shared entry.
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
