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

use mtui_config::Config;
use mtui_datasources::http::VerifyPolicy;
use mtui_datasources::refhost::{Attributes, Refhosts, RefhostsFactory, ResolveConfig, compare};
use mtui_hosts::{HostArbiter, HostError, HostsGroup, Owner, Prompter, Target};
use mtui_testreport::{TestReport, UpdateKind, make_testreport};
use mtui_types::UpdateID;
use mtui_types::enums::{ExecutionMode, TargetState, Workflow};
use tracing::{info, warn};

use crate::display::CommandPromptDisplay;
use crate::template_registry::TemplateRegistry;

/// The explicitly-passed state every command operates on.
pub struct Session {
    /// The application configuration.
    pub config: Config,
    /// Loaded templates and the active pointer.
    pub templates: TemplateRegistry,
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
    /// Per-slot candidate shuffle (upstream `random.shuffle`), so pool selection
    /// spreads load across interchangeable refhosts instead of always taking the
    /// first in `refhosts.yml` order. Defaults to a real random shuffle; tests
    /// override it with the identity ([`set_shuffle`](Self::set_shuffle)) for
    /// deterministic assertions.
    shuffle: ShuffleFn,
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
    pub fn parse(name: &str) -> Option<Self> {
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
        Self {
            config,
            templates,
            display: CommandPromptDisplay::stdout(),
            is_repl,
            should_exit: false,
            log_level_sink: None,
            notify_sink: None,
            prompter: None,
            shuffle: random_shuffle,
        }
    }

    /// Builds a session with an explicit display sink (test/embedding seam).
    #[must_use]
    pub fn with_display(config: Config, is_repl: bool, display: CommandPromptDisplay) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        Self {
            config,
            templates,
            display,
            is_repl,
            should_exit: false,
            log_level_sink: None,
            notify_sink: None,
            prompter: None,
            shuffle: random_shuffle,
        }
    }

    /// Overrides the per-slot candidate shuffle (test seam). Passing
    /// `|_| {}` (identity) makes pool selection deterministic.
    pub fn set_shuffle(&mut self, shuffle: ShuffleFn) {
        self.shuffle = shuffle;
    }

    /// The active report (upstream `prompt.metadata`). Never `None` — the
    /// [`TemplateRegistry`] returns a null object when nothing is loaded.
    #[must_use]
    pub fn metadata(&self) -> &(dyn TestReport + Send + Sync) {
        self.templates.active()
    }

    /// Mutably borrows the active report (upstream `prompt.metadata`, mutated).
    ///
    /// The mutable counterpart of [`metadata`](Self::metadata). The
    /// `reload_openqa` / `set_workflow` commands populate the report's openQA
    /// holder ([`TestReport::openqa_mut`]) through it; never `None` (the registry
    /// returns a null object when nothing is loaded).
    pub fn metadata_mut(&mut self) -> &mut (dyn TestReport + Send + Sync) {
        self.templates.active_mut().as_mut()
    }

    /// Sets the active report's [`Workflow`] mode (upstream
    /// `metadata.workflow = …`).
    ///
    /// The one mutable window onto the active report's workflow. `add_host`
    /// (and later `set_workflow`) uses it to move an automatic session to
    /// manual. Upstream additionally calls `prompt.set_prompt()` to refresh the
    /// REPL prompt string; that prompt refresh is a Phase-6 REPL concern, so the
    /// command only mutates the report here.
    pub fn set_workflow(&mut self, workflow: Workflow) {
        self.templates.active_mut().base_mut().workflow = workflow;
    }

    /// The active report's connected targets (upstream `prompt.targets`).
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.templates.active().base().targets
    }

    /// Mutably borrows the active report's connected targets.
    ///
    /// The mutable counterpart of [`targets`](Self::targets); command bodies
    /// that fan a command out across hosts (`run`, `reboot`, `set_repo`) need
    /// `&mut HostsGroup`.
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.templates.active_mut().base_mut().targets
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
    pub fn take_targets(&mut self) -> HostsGroup {
        let is_repl = self.is_repl;
        std::mem::replace(
            &mut self.templates.active_mut().base_mut().targets,
            HostsGroup::new(Vec::new(), is_repl),
        )
    }

    /// Restores the active report's targets, undoing [`take_targets`](Self::take_targets).
    pub fn restore_targets(&mut self, targets: HostsGroup) {
        self.templates.active_mut().base_mut().targets = targets;
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
    pub fn split_targets(
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
    pub fn restore_split_targets(&mut self, mut selected: HostsGroup, remainder: HostsGroup) {
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
    pub async fn load_update(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
    ) -> String {
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

        // `templates.add` ignores the empty-RRID null sentinel; a real report
        // becomes active (re-load replaces + re-activates).
        self.templates.add(report);
        if !rrid.is_empty() {
            self.templates.set_active(&rrid);
        }

        if pending && !rrid.is_empty() {
            self.autoconnect_active(&rrid).await;
        }
        rrid
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
            let base = self.templates.active().base();
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
        let lock_comment = self.templates.active().base().lock_comment.clone();
        // Snapshot the active report's package metadata (`product -> { name ->
        // required-version }`) before the `targets_mut()` borrow below. Cloning
        // it up front keeps the connect future `Send` (a `base()` borrow held
        // across the connect `.await` would not be) and is the port of upstream
        // `Target._parse_packages`, which seeds each host's tracked packages —
        // with their required versions — right after connect(). Empty when no
        // report (or a report with no packages) is loaded, in which case seeding
        // is a no-op, matching upstream's empty `self.packages` pre-load.
        let package_meta = self.templates.active().base().packages.clone();
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
        let pool_claims = self.templates.active().base().pool_claims.clone();
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
        let connected = futures::future::join_all(connect_futs).await;

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
            let base = self.templates.active().base();
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
                let base = self.templates.active_mut().base_mut();
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
                self.templates
                    .active_mut()
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
                        let base = self.templates.active_mut().base_mut();
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
        let warnings = self.templates.active_mut().base_mut();
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
    pub async fn add_testplatform_hosts(&mut self) {
        let config = self.config.clone();
        let shuffle = self.shuffle;
        let (already, testplatforms, arbiter, owner) = {
            let base = self.templates.active().base();
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
    pub async fn add_named_hosts(&mut self, hosts: Vec<String>) {
        let already = self.templates.active().base().targets.names();
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
    /// slot's candidates are shuffled (via the [`shuffle`](Self::set_shuffle)
    /// seam, upstream `random.shuffle`) and recorded so a failed connect can fall
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
            // order for stable iteration).
            let mut by_slot: Vec<(String, Vec<String>)> = Vec::new();
            for (host, slot) in pairs {
                let key = slot_key(&slot);
                match by_slot.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(host.name),
                    None => by_slot.push((key, vec![host.name])),
                }
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
                    let base = self.templates.active_mut().base_mut();
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
    pub fn apply_log_level(&mut self, level: LogLevel) -> bool {
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
    pub fn notify_user(&mut self, msg: &str, error: bool) -> bool {
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
        session.templates.set_active(rrid);
    }

    /// Reconstructs the legacy (non-pool) autoconnect host set from the active
    /// report: reference hosts merged with the `search()`-resolved testplatform
    /// hosts, minus the already-connected ones — the fallback path
    /// [`Session::resolve_and_record_pool`] takes when the arbiter is unwired.
    async fn autoconnect_hosts_of(s: &Session) -> Vec<String> {
        let config = s.config.clone();
        let (ref_hosts, already, testplatforms) = {
            let base = s.templates.active().base();
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
        let loaded = s.load_update(&update, true, UpdateKind::Auto).await;

        assert_eq!(loaded, "");
        assert!(s.templates.is_empty());
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
        s.templates
            .active_mut()
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

        let base = s.templates.active().base();
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

    /// The composition root wires the arbiter + owner onto every added report
    /// (`_pool_selection_active`), so autoconnect takes the pool path.
    #[test]
    fn added_report_has_arbiter_and_owner_wired() {
        let mut s = Session::new(config(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let base = s.templates.active().base();
        assert!(base.arbiter.is_some(), "arbiter must be wired on add()");
        let owner = base.owner.as_ref().expect("owner wired");
        assert_eq!(
            owner.1, "SUSE:Maintenance:1:1",
            "owner RRID is the report id"
        );
    }
}
