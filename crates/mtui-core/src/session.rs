//! Shared, explicitly-passed command state (`Session`).
//!
//! Commands receive `&mut Session` and mutate it through methods — no hidden
//! globals. It owns the [`Config`], the [`TemplateRegistry`], the
//! [`CommandPromptDisplay`] sink, and the flag distinguishing the REPL from
//! headless callers such as `mtui-mcp`. [`metadata`](Session::metadata) /
//! [`targets`](Session::targets) delegate to the active report.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mtui_config::Config;
use mtui_datasources::HttpError;
use mtui_datasources::http::{HttpClient, VerifyPolicy};
use mtui_datasources::refhost::{Attributes, Refhosts, RefhostsFactory, ResolveConfig, compare};
use mtui_datasources::teregen::TokenStore;
#[cfg(test)]
use mtui_hosts::MockConnection;
use mtui_hosts::{
    Connection, HostArbiter, HostError, HostsGroup, LockOutcome, OpLockHolders, Owner, Prompter,
    Target,
};
use mtui_testreport::reports::CommandSetGap;
use mtui_testreport::{NullReport, TestReport, UpdateKind, make_testreport};
use mtui_types::UpdateID;
use mtui_types::enums::{TargetState, Workflow};
use mtui_types::hostlog::HostLog;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::display::CommandPromptDisplay;
use crate::error::CommandError;
use crate::template_registry::{ReportEntry, TemplateRegistry, TemplateRow};

/// Wall-clock budget for a host-close fan-out.
///
/// A wedged teardown (a dead peer whose close never returns) must not block the
/// REPL `quit`, template replacement/removal, or the MCP idle-sweep: each bounds
/// its fan-out with this budget and abandons the straggler.
pub const HOST_CLOSE_TIMEOUT: Duration = Duration::from_secs(45);

/// The `[lock]` wait and poll budgets as the signed seconds the arbiter takes.
fn lock_wait_secs(config: &Config) -> (i64, i64) {
    (
        i64::try_from(config.lock_wait).unwrap_or(i64::MAX),
        i64::try_from(config.lock_wait_poll).unwrap_or(i64::MAX),
    )
}

/// Cache slot for [`Session::http_client`], shared across [`fork_for_call`](Session::fork_for_call).
type HttpClientCache = Arc<Mutex<Option<(VerifyPolicy, HttpClient)>>>;

/// Cache slot for [`Session::openqa_transport`], shared across
/// [`fork_for_call`](Session::fork_for_call).
type OpenqaTransportCache = Arc<Mutex<Option<(VerifyPolicy, reqwest::Client)>>>;

/// Command logs kept after their host was removed, keyed by
/// `(rrid, hostname)`.
type DetachedLogs = BTreeMap<(String, String), Arc<HostLog>>;

/// One command-log entry mapped for display: `(command, stdout, stderr, exit)`.
pub(crate) type LogEntry = (String, String, String, i32);

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
    /// when present and through [`null`](Self::null) otherwise.
    /// [`Command::run`](crate::Command::run) installs it per resolved template,
    /// dropping any prior guard *first* so one [`Session`] never self-deadlocks.
    active_guard: Option<OwnedMutexGuard<Box<dyn TestReport + Send + Sync>>>,
    /// The null-object fallback [`metadata`](Self::metadata) hands out when
    /// nothing is loaded. Never registered, so hosts attached while nothing is
    /// loaded are reachable only via
    /// [`take_teardown_units`](Self::take_teardown_units).
    null: Box<dyn TestReport + Send + Sync>,
    /// Formatted-output sink.
    pub display: CommandPromptDisplay,
    /// `true` for the interactive REPL, `false` for headless callers (MCP),
    /// where there is no `switch` and an unscoped command fans out.
    pub is_repl: bool,
    /// Set by `quit` to ask the REPL loop to exit after the current dispatch;
    /// MCP ignores it. See [`should_exit`](Self::should_exit).
    should_exit: bool,
    /// Optional sink for runtime log-level changes (a `tracing_subscriber::reload`
    /// handle); `None` in tests, where `set_log_level` fails rather than claim a
    /// change.
    log_level_sink: Option<LogLevelSink>,
    /// Optional sink for best-effort desktop notifications, set by the REPL
    /// only, which keeps `mtui-core` free of the CLI notification backend.
    notify_sink: Option<NotifySink>,
    /// The session-level serialised interactive [`Prompter`], or `None`
    /// headlessly, where a command timeout aborts immediately.
    ///
    /// Installed via [`set_prompter`](Self::set_prompter); it reaches each
    /// freshly-built [`Target`] in
    /// [`connect_and_add_hosts`](Self::connect_and_add_hosts) and the active
    /// report's [`HostsGroup`].
    prompter: Option<Prompter>,
    /// Test-only count of [`http_client`](Self::http_client) *builds* (not cached
    /// clones), shared with every fork so per-fork caching shows up.
    #[cfg(test)]
    http_builds: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only build count for [`openqa_transport`](Self::openqa_transport).
    #[cfg(test)]
    openqa_builds: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only home of the teregen token cache. Shared with every fork.
    #[cfg(test)]
    teregen_token_dir: Arc<tempfile::TempDir>,
    /// Test-only dial replacement (see [`inject_mock_connection`](Self::inject_mock_connection)).
    #[cfg(test)]
    mock_conns: std::collections::HashMap<String, MockConnection>,
    /// Per-slot candidate shuffle, so pool selection spreads load across
    /// interchangeable refhosts. Tests override it with the identity.
    shuffle: ShuffleFn,
    /// The cancellation seam: cooperative cancel signal for the dispatch this
    /// session is driving (MCP `job_cancel`).
    ///
    /// [`fork_for_call`](Self::fork_for_call) *clones* it, and `mtui-mcp`
    /// installs a per-job token via [`set_cancel_token`](Self::set_cancel_token).
    /// [`Command::run`](crate::Command::run) checks it before dispatch and
    /// between fan-out templates. Purely cooperative: a body that never checks is
    /// hard-aborted by the MCP job layer after its grace period.
    cancel: CancellationToken,
    /// Lazily-built, session-scoped outbound [`HttpClient`], cached with the
    /// [`VerifyPolicy`] it was built under.
    ///
    /// Built once and cloned (a per-command client means a cold pool per
    /// command), rebuilding only when the posture changes. Interior mutability
    /// lets `&Session` call sites populate it lazily. The `Arc` is **shared**
    /// with every [`fork_for_call`](Self::fork_for_call) descendant: the MCP
    /// canonical session never dispatches directly, so a per-fork slot would
    /// never warm. The guard is held across the build deliberately, so
    /// concurrent cold forks convoy on one build instead of doing N.
    http_client: HttpClientCache,
    /// Lazily-built, session-scoped openQA transport (a redirect-less,
    /// no-reqwest-retry `reqwest::Client`), cached and shared across forks like
    /// [`http_client`](Self::http_client) so back-to-back connectors share a pool.
    openqa_transport: OpenqaTransportCache,
    /// Command logs kept after their host was removed, keyed by
    /// `(rrid, hostname)`, so `show_log` still answers until `unload`.
    ///
    /// A `Target` owns the only verbatim record of what ran on it, so dropping it
    /// (via `remove_host`) must not destroy the session's copy. Shared with every
    /// [`fork_for_call`](Self::fork_for_call), and dropped with its template by
    /// [`forget_detached_logs`](Self::forget_detached_logs).
    detached_logs: Arc<Mutex<DetachedLogs>>,
    /// The registry through which every target in this process sees the
    /// operation locks its siblings hold.
    ///
    /// Wire ownership is per user + PID, so two templates (or MCP sessions)
    /// sharing a refhost would otherwise read each other's lock as their own.
    /// Shared with every [`fork_for_call`](Self::fork_for_call); the MCP registry
    /// hands one instance to all its sessions via
    /// [`set_op_lock_holders`](Self::set_op_lock_holders). Pushed onto the active
    /// group on every [`activate`](Self::activate) and onto each target before it
    /// connects.
    op_lock_holders: OpLockHolders,
}

/// Maximum matching of slots to the hosts in `free` by augmenting paths,
/// trying an unmatched host first so earlier slots and candidates win ties.
/// The hosts `first(slot, host)` accepts are matched before the rest and stay
/// matched, so as many of them serve as any maximum matching allows.
fn match_slots<'a>(
    free: &[Vec<&'a String>],
    first: impl Fn(usize, &String) -> bool,
) -> Vec<Option<&'a String>> {
    use std::collections::{HashMap, HashSet};
    fn augment<'a>(
        slot: usize,
        free: &[Vec<&'a String>],
        taken: &mut HashMap<&'a String, usize>,
        seen: &mut HashSet<&'a String>,
    ) -> bool {
        if let Some(&host) = free[slot].iter().find(|h| !taken.contains_key(*h)) {
            taken.insert(host, slot);
            return true;
        }
        for &host in &free[slot] {
            if seen.insert(host) && augment(taken[host], free, taken, seen) {
                taken.insert(host, slot);
                return true;
            }
        }
        false
    }
    let preferred: Vec<Vec<&'a String>> = free
        .iter()
        .enumerate()
        .map(|(slot, hosts)| hosts.iter().copied().filter(|h| first(slot, h)).collect())
        .collect();
    let mut taken = HashMap::new();
    let matched_first: Vec<bool> = (0..free.len())
        .map(|slot| augment(slot, &preferred, &mut taken, &mut HashSet::new()))
        .collect();
    for slot in (0..free.len()).filter(|s| !matched_first[*s]) {
        augment(slot, free, &mut taken, &mut HashSet::new());
    }
    let mut matched = vec![None; free.len()];
    for (host, slot) in taken {
        matched[slot] = Some(host);
    }
    matched
}

/// A candidate-order shuffle seam: mutates the slot's candidate list in place
/// before the arbiter picks one.
pub type ShuffleFn = fn(&mut [String]);

/// A claim seam over [`HostArbiter::try_acquire`], so a test can lose a
/// matched host to another claim.
type ClaimFn = fn(&HostArbiter, &str, &Owner) -> bool;

/// The default [`ShuffleFn`]: a real random shuffle.
fn random_shuffle(candidates: &mut [String]) {
    use rand::seq::SliceRandom;
    candidates.shuffle(&mut rand::rng());
}

/// Renders a refhosts [`Slot`](mtui_datasources::refhost::Slot) as the
/// `product|version|arch|addon,addon` key
/// [`TestReportBase::slot_candidates`](mtui_testreport::TestReportBase::slot_candidates)
/// groups backup candidates under.
fn slot_key(slot: &mtui_datasources::refhost::Slot) -> String {
    let (product, version, arch, addons) = slot;
    format!("{product}|{version}|{arch}|{}", addons.join(","))
}

/// The active report's seed packages, taken before a `targets_mut()` borrow so
/// the futures that seed hosts borrow plain data and stay `Send`.
pub(crate) struct PackageMeta(Arc<mtui_testreport::HostPackages>);

impl PackageMeta {
    pub(crate) fn of(base: &mtui_testreport::TestReportBase) -> Self {
        Self(Arc::clone(&base.host_packages))
    }

    /// Whether the report carries no package metadata at all.
    pub(crate) fn is_empty(&self) -> bool {
        match &*self.0 {
            mtui_testreport::HostPackages::Shared(set) => set.is_empty(),
            mtui_testreport::HostPackages::PerProduct(by_product) => by_product.is_empty(),
        }
    }

    /// Points `target`'s tracked packages at the report targets matching its
    /// current products and returns what changed. Their installed versions are
    /// left to [`Target::query_versions`].
    ///
    /// A package `target` already tracks keeps its recorded versions and takes
    /// the new required version when the seed carries one. A package no longer
    /// named is dropped unless it carries a recorded check: that one stays as it
    /// was, so a product change cannot erase the evidence `export` verifies.
    pub(crate) fn reseed(&self, target: &mut Target) -> Reseed {
        let seeded = mtui_testreport::testreport::packages_for_host(&self.0, target.system());
        let mut tracked: BTreeMap<String, mtui_types::package::Package> = target
            .packages()
            .iter()
            .map(|p| (p.name.clone(), p.clone()))
            .collect();
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut packages: Vec<_> = seeded
            .into_iter()
            .map(|pkg| match tracked.remove(&pkg.name) {
                Some(mut kept) => {
                    if let Some(required) = pkg.required().cloned()
                        && kept.required() != Some(&required)
                    {
                        updated.push((
                            kept.name.clone(),
                            kept.required().cloned(),
                            required.clone(),
                        ));
                        kept.set_required_version(required);
                    }
                    kept
                }
                None => {
                    added.push(pkg.name.clone());
                    pkg
                }
            })
            .collect();
        let (checked, dropped): (Vec<_>, Vec<_>) = tracked
            .into_values()
            .partition(|p| p.before_check().is_checked() || p.after_check().is_checked());
        let names = |pkgs: &[mtui_types::package::Package]| -> Vec<String> {
            pkgs.iter().map(|p| p.name.clone()).collect()
        };
        let summary = Reseed {
            added,
            updated,
            dropped: names(&dropped),
            kept: names(&checked),
        };
        packages.extend(checked);
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        if packages.is_empty() {
            // Tracking nothing means version checks cannot run: say so.
            warn!(
                host = %target.hostname(), system = %target.system(),
                "report metadata names no packages for this host's products; \
                 package list not seeded — version checks cannot run"
            );
        }
        target.set_packages(packages);
        summary
    }
}

/// What [`PackageMeta::reseed`] changed in a host's tracked packages, each
/// list sorted by name.
#[derive(Debug)]
pub(crate) struct Reseed {
    /// Newly tracked, so never checked.
    pub(crate) added: Vec<String>,
    /// Still tracked, with a new required version: `(name, old, new)`.
    pub(crate) updated: Vec<(
        String,
        Option<mtui_types::RPMVersion>,
        mtui_types::RPMVersion,
    )>,
    /// No longer tracked.
    pub(crate) dropped: Vec<String>,
    /// No longer named by the seed, but still tracked for their recorded checks.
    pub(crate) kept: Vec<String>,
}

/// Why a single host could not be attached to its slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostFailure {
    Unreachable,
    /// Another process holds the remote pool claim; the claiming RRID when it
    /// could be read back.
    ClaimedBy(Option<String>),
    /// The host connected but writing its remote pool claim failed.
    ClaimFailed,
    /// The host connected while an update was loaded, but has no
    /// update-workflow command set. `drift` is its refhosts.yml product drift,
    /// which would otherwise be lost with the host.
    Unusable {
        gap: CommandSetGap,
        drift: Option<Vec<String>>,
    },
    /// A pool-drawn host whose live operation lock another owner holds.
    OperationLocked {
        by: String,
        since: String,
    },
}

impl HostFailure {
    pub(crate) fn render(&self) -> String {
        match self {
            Self::Unreachable => "unreachable".to_owned(),
            Self::ClaimedBy(Some(rrid)) => format!("claimed by {rrid}"),
            Self::ClaimedBy(None) => "claimed remotely".to_owned(),
            Self::ClaimFailed => "pool claim failed".to_owned(),
            Self::Unusable { gap, .. } => gap.to_string(),
            Self::OperationLocked { by, since } => {
                format!("operation lock held by {by} since {since}")
            }
        }
    }

    /// Whether attach refused the host rather than failing to take it, so
    /// the refusal is named even when a sibling served its slot.
    fn is_refusal(&self) -> bool {
        match self {
            Self::Unusable { .. } | Self::OperationLocked { .. } => true,
            Self::Unreachable | Self::ClaimedBy(_) | Self::ClaimFailed => false,
        }
    }
}

/// Each host attach refused ([`HostFailure::is_refusal`]), once and sorted by
/// host, whether as a primary (`failures`) or a backup sibling (`unserved`).
fn refused_hosts<'a>(
    failures: &'a [(String, HostFailure)],
    unserved: &'a [Unserved],
) -> Vec<(&'a str, &'a HostFailure)> {
    let backups = unserved.iter().flat_map(Unserved::host_failures);
    let refused: std::collections::BTreeMap<&str, &HostFailure> = failures
        .iter()
        .chain(backups)
        .filter(|(_, f)| f.is_refusal())
        .map(|(h, f)| (h.as_str(), f))
        .collect();
    refused.into_iter().collect()
}

/// A test-target slot, or a testplatform, that `add_host` left without a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unserved {
    NoRefhost {
        tp: String,
    },
    /// One requested arch of a testplatform whose other arches matched.
    NoArchRefhost {
        tp: String,
        arch: String,
    },
    InventoryUnavailable,
    /// Every candidate is claimed in-process by another RRID,
    /// `(host, claiming RRID)`, or already serves another slot (`shared`).
    Busy {
        slot: String,
        holders: Vec<(String, String)>,
        shared: Vec<String>,
    },
    /// Every candidate already serves another slot of this update.
    Shared {
        slot: String,
        hosts: Vec<String>,
    },
    Unconnectable {
        slot: String,
        failures: Vec<(String, HostFailure)>,
    },
    BudgetSpent {
        slot: String,
        tried: Vec<String>,
    },
}

impl Unserved {
    /// The hosts this line gives a reason for, with that reason.
    fn host_failures(&self) -> &[(String, HostFailure)] {
        match self {
            Self::Unconnectable { failures, .. } => failures,
            Self::NoRefhost { .. }
            | Self::NoArchRefhost { .. }
            | Self::InventoryUnavailable
            | Self::Busy { .. }
            | Self::Shared { .. }
            | Self::BudgetSpent { .. } => &[],
        }
    }

    /// The one-line, human-facing reason.
    pub(crate) fn render(&self) -> String {
        match self {
            Self::NoRefhost { tp } => format!("testplatform \"{tp}\": no matching refhost"),
            Self::NoArchRefhost { tp, arch } => {
                format!("testplatform \"{tp}\": no refhost for arch {arch}")
            }
            Self::InventoryUnavailable => {
                "refhosts inventory unavailable; no testplatform host resolved".to_owned()
            }
            Self::Busy {
                slot,
                holders,
                shared,
            } => {
                let by = holders
                    .iter()
                    .map(|(h, r)| format!("{h} by {r}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let also = if shared.is_empty() {
                    String::new()
                } else {
                    format!(" or already serving another slot ({})", shared.join(", "))
                };
                format!(
                    "unserved slot {}: all {} candidates claimed ({by}){also}",
                    slot_label(slot),
                    holders.len() + shared.len()
                )
            }
            Self::Shared { slot, hosts } => format!(
                "unserved slot {}: every candidate already serves another slot ({})",
                slot_label(slot),
                hosts.join(", ")
            ),
            Self::Unconnectable { slot, failures } => {
                let why = failures
                    .iter()
                    .map(|(h, f)| format!("{h}: {}", f.render()))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!(
                    "unserved slot {}: no candidate connected ({why})",
                    slot_label(slot)
                )
            }
            Self::BudgetSpent { slot, tried } => format!(
                "unserved slot {}: backup retry budget exhausted (tried {})",
                slot_label(slot),
                tried.join(", ")
            ),
        }
    }
}

/// A [`slot_key`] for display: `product version arch[ +addon…]`.
fn slot_label(key: &str) -> String {
    let mut parts = key.splitn(4, '|');
    let (Some(product), Some(version), Some(arch), Some(addons)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return key.to_owned();
    };
    let mut label = format!("{product} {version} {arch}");
    for addon in addons.split(',').filter(|a| !a.is_empty()) {
        label.push_str(" +");
        label.push_str(addon);
    }
    label
}

/// The outcome of [`Session::activate`].
///
/// Only [`Active`](Self::Active) installs the per-call guard; the other three
/// leave `metadata()` on the null sentinel and are *not* interchangeable:
/// [`Empty`](Self::Empty) is the legitimate nothing-loaded session, while the
/// other two are races a caller must refuse on rather than answer from the
/// sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// The entry was claimed; `metadata()` is that template's report.
    Active,
    /// Empty rrid: pointer and guard cleared on purpose.
    Empty,
    /// No registry entry under that rrid — unloaded between resolve and claim.
    NotLoaded,
    /// The entry is locked elsewhere, so `try_lock_owned` gave up.
    Busy,
}

impl Activation {
    /// Whether the per-call guard was installed.
    #[must_use]
    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

/// Why [`Session::with_report`] could not reach a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportAccess {
    /// No registry entry under that rrid.
    NotLoaded,
    /// The entry is locked by another holder.
    Busy,
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
    /// Builds a session for `config` on stdout; `is_repl` is `true` for the
    /// REPL, `false` for MCP.
    #[must_use]
    pub fn new(config: Config, is_repl: bool) -> Self {
        Self::with_display(config, is_repl, CommandPromptDisplay::stdout())
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
            http_client: Arc::new(Mutex::new(None)),
            openqa_transport: Arc::new(Mutex::new(None)),
            detached_logs: Arc::new(Mutex::new(BTreeMap::new())),
            op_lock_holders: OpLockHolders::default(),
            #[cfg(test)]
            http_builds: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            openqa_builds: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            teregen_token_dir: Arc::new(tempfile::tempdir().expect("temp token dir")),
            #[cfg(test)]
            mock_conns: std::collections::HashMap::new(),
        }
    }

    /// Builds a cheap per-call [`Session`] that **shares** this session's loaded
    /// reports and carries its own display sink.
    ///
    /// The headless MCP concurrency seam: holding the canonical session behind a
    /// mutex across dispatch would serialise *all* calls. A fork's
    /// `TemplateRegistry::snapshot` shares the per-entry report locks, so a
    /// command on RRID `X` locks only `X`'s entry and its mutations stay visible
    /// to the canonical session.
    ///
    /// `config`/`is_repl`/`shuffle` are copied **by value**, so a fork's writes to
    /// them die with it; a command that mutates any of them must declare
    /// [`Command::requires_canonical_session`](crate::Command::requires_canonical_session).
    /// The `http_client`/`openqa_transport` caches are **shared** with the
    /// parent, like the report entry locks and the cancel token, so every call
    /// need not pay a fresh TLS build. That is sound because a posture change
    /// lands only through the MCP exclusive arm, which drains every live fork
    /// first, and the `VerifyPolicy` cache key turns a residual mismatch into a
    /// rebuild. Only the prompter/sinks start empty. Only a
    /// **single-real-template** command that mutates nothing beyond shared report
    /// content is sound to dispatch through a fork.
    #[must_use]
    pub fn fork_for_call(&self, display: CommandPromptDisplay) -> Self {
        // A host added on a fork while nothing is loaded would land in this
        // private `null` and be lost with the fork. Two mechanisms keep that
        // unreachable: nothing loaded never forks (`resolve_command_rrids`
        // returns `None`, which `command_lock` maps to the exclusive arm), and
        // that exclusive path releases the canonical active guard on the way out
        // so a later fork's `activate` can re-lock the entry. The second half is
        // enforced in `mtui-mcp`.
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
            // Cancelling either side must be observable on both.
            cancel: self.cancel.clone(),
            http_client: Arc::clone(&self.http_client),
            openqa_transport: Arc::clone(&self.openqa_transport),
            detached_logs: Arc::clone(&self.detached_logs),
            op_lock_holders: self.op_lock_holders.clone(),
            #[cfg(test)]
            http_builds: Arc::clone(&self.http_builds),
            #[cfg(test)]
            openqa_builds: Arc::clone(&self.openqa_builds),
            #[cfg(test)]
            teregen_token_dir: Arc::clone(&self.teregen_token_dir),
            #[cfg(test)]
            mock_conns: self.mock_conns.clone(),
        }
    }

    /// The registry through which this session's targets see their siblings'
    /// operation locks.
    #[must_use]
    pub fn op_lock_holders(&self) -> &OpLockHolders {
        &self.op_lock_holders
    }

    /// Joins this session to a registry shared with other sessions, and pushes
    /// it onto the groups it already owns.
    ///
    /// An entry locked elsewhere is skipped; the next [`activate`](Self::activate)
    /// of it installs the registry.
    pub fn set_op_lock_holders(&mut self, holders: OpLockHolders) {
        self.null
            .base_mut()
            .targets
            .set_op_lock_holders(holders.clone());
        for rrid in self.templates.rrids() {
            let Some(entry) = self.templates.handle(&rrid) else {
                continue;
            };
            if let Ok(mut report) = entry.try_lock() {
                report
                    .base_mut()
                    .targets
                    .set_op_lock_holders(holders.clone());
            }
        }
        self.op_lock_holders = holders;
    }

    /// `true` once cancellation has been requested for this dispatch.
    ///
    /// The poll form, for a body checking at its own boundaries;
    /// [`check_cancelled`](Self::check_cancelled) is the `?`-friendly form and
    /// [`cancel_token`](Self::cancel_token) the awaitable one.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Bail with [`CommandError::Cancelled`] if cancellation has been requested.
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
    /// The MCP job layer installs one token per background job, so `job_cancel`
    /// cancels exactly that job. It *replaces* rather than chains, so a caller
    /// whose session outlives the call must restore the prior token.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel = token;
    }

    /// The session-scoped outbound [`HttpClient`], built lazily and reused.
    ///
    /// The [`VerifyPolicy`] comes from `config.ssl_verify` and keys the cache, so
    /// a mid-session `config set ssl_verify` rebuilds. Later calls return a cheap
    /// clone sharing one `reqwest` connection pool.
    ///
    /// # Errors
    ///
    /// Propagates [`HttpError`] when the client cannot be built (e.g. an
    /// unreadable CA bundle).
    pub(crate) fn http_client(&self) -> Result<HttpClient, HttpError> {
        self.cached(&self.http_client, |policy| {
            let client = HttpClient::new(policy.clone())?;
            #[cfg(test)]
            self.http_builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(client)
        })
    }

    /// The cached value in `slot` when it was built under the current
    /// [`VerifyPolicy`], otherwise a fresh `build`. Keying on the policy turns a
    /// mid-session `config set ssl_verify` into a rebuild, never a stale hand-out.
    fn cached<T: Clone>(
        &self,
        slot: &Mutex<Option<(VerifyPolicy, T)>>,
        build: impl FnOnce(&VerifyPolicy) -> Result<T, HttpError>,
    ) -> Result<T, HttpError> {
        let policy = VerifyPolicy::from_config(&self.config.ssl_verify);
        let mut cache = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_policy, value)) = cache.as_ref()
            && *cached_policy == policy
        {
            return Ok(value.clone());
        }
        let value = build(&policy)?;
        *cache = Some((policy, value.clone()));
        Ok(value)
    }

    /// The teregen token cache: the operator's data dir in production, a
    /// per-session temp dir under test.
    #[cfg(not(test))]
    pub(crate) fn teregen_token_store(&self) -> Option<TokenStore> {
        TokenStore::new()
    }

    /// The teregen token cache: the operator's data dir in production, a
    /// per-session temp dir under test.
    #[cfg(test)]
    pub(crate) fn teregen_token_store(&self) -> Option<TokenStore> {
        Some(TokenStore::at(
            self.teregen_token_dir.path().join("teregen-token.json"),
        ))
    }

    /// Test-only count of clients built by [`http_client`](Self::http_client).
    #[cfg(test)]
    pub(crate) fn http_builds(&self) -> usize {
        self.http_builds.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test-only count of transports built by
    /// [`openqa_transport`](Self::openqa_transport).
    #[cfg(test)]
    fn openqa_builds(&self) -> usize {
        self.openqa_builds.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Registers a mock connection `host` dials instead of SSH (test seam for
    /// [`connect_and_add_hosts`](Self::connect_and_add_hosts) and its backup
    /// siblings).
    #[cfg(test)]
    pub(crate) fn inject_mock_connection(&mut self, host: &str, conn: MockConnection) {
        self.mock_conns.insert(host.to_owned(), conn);
    }

    /// The registered dial replacements, by host; always empty in production.
    #[cfg(test)]
    fn injected_connections(&self) -> std::collections::HashMap<String, Box<dyn Connection>> {
        self.mock_conns
            .iter()
            .map(|(h, c)| (h.clone(), Box::new(c.clone()) as Box<dyn Connection>))
            .collect()
    }

    /// The registered dial replacements, by host; always empty in production.
    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn injected_connections(&self) -> std::collections::HashMap<String, Box<dyn Connection>> {
        std::collections::HashMap::new()
    }

    /// The session-scoped openQA transport, built lazily and reused.
    ///
    /// Caches by [`VerifyPolicy`] like [`http_client`](Self::http_client), but
    /// hands out a bare `reqwest::Client`: `ruoqa` re-signs and redirects every
    /// request itself, so the transport must not follow redirects or retry (see
    /// [`HttpClient::openqa_transport`]).
    ///
    /// # Errors
    ///
    /// Propagates [`HttpError`] when the transport cannot be built (e.g. an
    /// unreadable CA bundle).
    pub(crate) fn openqa_transport(&self) -> Result<reqwest::Client, HttpError> {
        self.cached(&self.openqa_transport, |policy| {
            let transport = HttpClient::openqa_transport(policy.clone())?;
            #[cfg(test)]
            self.openqa_builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(transport)
        })
    }

    /// Makes `rrid` the active template *and* installs its per-call active
    /// handle (`active_guard`).
    ///
    /// Any prior guard is dropped first, so re-activating the same entry never
    /// self-deadlocks. An empty `rrid` clears pointer and guard so `metadata()`
    /// falls back to the null object.
    ///
    /// `#[must_use]`: ignoring a lost race silently redirects the dispatch onto
    /// the sentinel; only [`Empty`](Activation::Empty) is a legitimate
    /// non-[`Active`](Activation::Active) outcome.
    #[must_use = "a non-Active activation leaves the dispatch on the null report"]
    pub fn activate(&mut self, rrid: &str) -> Activation {
        self.active_guard = None;
        self.null
            .base_mut()
            .targets
            .set_op_lock_holders(self.op_lock_holders.clone());
        // Hosts added with no template loaded live here.
        self.null
            .base_mut()
            .targets
            .set_cancel_token(self.cancel.clone());
        if rrid.is_empty() {
            self.templates.set_active_none();
            return Activation::Empty;
        }
        if !self.templates.set_active(rrid) {
            return Activation::NotLoaded;
        }
        self.active_guard = self
            .templates
            .active_handle()
            .and_then(|h| h.try_lock_owned().ok());
        // Every dispatch passes through here, so every command gets the
        // host-boundary seam, refreshed so a group never keeps a stale token.
        let cancel = self.cancel.clone();
        let holders = self.op_lock_holders.clone();
        if let Some(guard) = self.active_guard.as_mut() {
            let targets = &mut guard.base_mut().targets;
            targets.set_cancel_token(cancel);
            targets.set_op_lock_holders(holders);
        }
        if self.active_guard.is_some() {
            Activation::Active
        } else {
            Activation::Busy
        }
    }

    /// Drops the per-call active handle *without* changing the active pointer.
    ///
    /// Teardown/probe paths that lock entries directly would self-deadlock on an
    /// entry this session's guard still holds. Unlike `activate("")` the
    /// registry's active RRID is left intact.
    pub fn release_active_guard(&mut self) {
        self.active_guard = None;
    }

    /// Every host group this session owns, as independently-lockable teardown
    /// units: one per registry entry, plus the sentinel's group when it holds
    /// hosts.
    ///
    /// The single enumeration teardown walks (`quit`, `McpSession::close`, the
    /// MCP idle sweep). The registry alone is *not* the set of connected hosts:
    /// `add_host` with nothing loaded writes into the private sentinel. A
    /// non-empty sentinel group is **moved out** (swapped for a fresh
    /// [`NullReport`]) so it can outlive the caller's borrow and cannot be handed
    /// out twice. The per-call active handle is released first, since teardown
    /// locks *every* entry.
    ///
    /// # Warning
    ///
    /// Dropping the returned `Vec` instead of tearing down every unit strands the
    /// sentinel's hosts with their remote `/var/lock/mtui.lock` held.
    #[must_use]
    pub fn take_teardown_units(&mut self) -> Vec<ReportEntry> {
        self.release_active_guard();
        // The sentinel goes first: under a shared deadline it is the only
        // unrecoverable unit.
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

    /// Whether the null sentinel's host group currently holds any hosts.
    ///
    /// The sentinel is the only host set the registry walk
    /// ([`TemplateRegistry::rrids`]) cannot see; callers enumerating every group
    /// that could hold a lock (see
    /// [`take_teardown_units`](Self::take_teardown_units),
    /// [`unlock_null_group_held`](Self::unlock_null_group_held)) must include it.
    #[must_use]
    pub fn null_group_has_hosts(&self) -> bool {
        !self.null.base().targets.is_empty()
    }

    /// The null sentinel's host group, whatever active guard is installed —
    /// unlike [`targets`](Self::targets), which follows the active report.
    #[must_use]
    pub fn null_targets(&self) -> &HostsGroup {
        &self.null.base().targets
    }

    /// Releases the operation locks the null sentinel's own targets took.
    ///
    /// The mid-session analogue of [`take_teardown_units`](Self::take_teardown_units):
    /// the session continues, so this drives the sentinel's
    /// [`HostsGroup::unlock_held`] in place. That scopes to locks this group's own
    /// targets took, leaving a foreign or comment-marked hold untouched.
    ///
    /// The caller holds the session mutex across this call; the fan-out's
    /// per-host timeouts bound it.
    pub async fn unlock_null_group_held(&mut self) -> BTreeMap<String, LockOutcome> {
        self.null.base_mut().targets.unlock_held().await
    }

    /// Re-installs the active handle for the registry's current active pointer.
    ///
    /// Used after a registry mutation repoints `active` without going through
    /// [`activate`](Self::activate): drops any stale guard and locks the
    /// (possibly new) active entry.
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
    /// Lets guard-unaware callers (the MCP `report_*` tools) choose between
    /// [`metadata`](Self::metadata) and locking the entry handle.
    #[must_use]
    pub fn active_report_is_guarded(&self, rrid: &str) -> bool {
        self.active_guard.is_some() && self.templates.active_rrid() == Some(rrid)
    }

    /// Reads `f` off the report under `rrid` for the fan-out probes: `absent`
    /// when it is not loaded, `None` when another holder has it. A contended
    /// entry must never read as absent, or a fan-out would skip a template racing
    /// another dispatch instead of refusing it.
    fn probe_report<R>(
        &self,
        rrid: &str,
        absent: R,
        f: impl FnOnce(&(dyn TestReport + Send + Sync)) -> R,
    ) -> Option<R> {
        match self.with_report(rrid, f) {
            Ok(v) => Some(v),
            Err(ReportAccess::NotLoaded) => Some(absent),
            Err(ReportAccess::Busy) => None,
        }
    }

    /// Whether the report loaded under `rrid` has no connected hosts, or `None`
    /// when the entry is held elsewhere.
    ///
    /// The active entry is read through this session's
    /// [`active_guard`](Self::active_guard); a foreign holder still yields `None`.
    #[must_use]
    pub(crate) fn is_hostless(&self, rrid: &str) -> Option<bool> {
        self.probe_report(rrid, true, |r| r.base().targets.is_empty())
    }

    /// Whether the report loaded under `rrid` holds uncommitted document edits,
    /// or `None` when the entry is held elsewhere (guard-aware, like
    /// [`is_hostless`](Self::is_hostless)).
    #[must_use]
    pub(crate) fn document_dirty(&self, rrid: &str) -> Option<bool> {
        self.probe_report(rrid, false, |r| r.base().document_dirty)
    }

    /// Runs `f` on the report loaded under `rrid`.
    ///
    /// Guard-aware: the active entry is read through its held guard, any other
    /// through a `try_lock`.
    ///
    /// # Errors
    ///
    /// [`ReportAccess::NotLoaded`] when `rrid` is not loaded,
    /// [`ReportAccess::Busy`] when another holder has the entry.
    pub fn with_report<R>(
        &self,
        rrid: &str,
        f: impl FnOnce(&(dyn TestReport + Send + Sync)) -> R,
    ) -> Result<R, ReportAccess> {
        if self.active_report_is_guarded(rrid) {
            return Ok(f(self.metadata()));
        }
        let handle = self.templates.handle(rrid).ok_or(ReportAccess::NotLoaded)?;
        let report = handle.try_lock().map_err(|_| ReportAccess::Busy)?;
        Ok(f(&**report))
    }

    /// The mutable counterpart of [`with_report`](Self::with_report). A contended
    /// entry is [`ReportAccess::Busy`], never the null sentinel.
    pub fn with_report_mut<R>(
        &mut self,
        rrid: &str,
        f: impl FnOnce(&mut (dyn TestReport + Send + Sync)) -> R,
    ) -> Result<R, ReportAccess> {
        if self.active_report_is_guarded(rrid) {
            return Ok(f(self.metadata_mut()));
        }
        let handle = self.templates.handle(rrid).ok_or(ReportAccess::NotLoaded)?;
        let mut report = handle.try_lock().map_err(|_| ReportAccess::Busy)?;
        Ok(f(&mut **report))
    }

    /// Whether none of `named` belong to `rrid`'s host group, or `None` when the
    /// entry is held elsewhere (guard-aware, like
    /// [`is_hostless`](Self::is_hostless)).
    ///
    /// Live hosts only: commands reading detached logs consult
    /// [`detached_snapshot`](Self::detached_snapshot) through
    /// [`Command::includes_detached_hosts`](crate::Command::includes_detached_hosts).
    #[must_use]
    pub(crate) fn owns_none_of(&self, rrid: &str, named: &[String]) -> Option<bool> {
        self.probe_report(rrid, true, |r| {
            !named.iter().any(|h| r.base().targets.contains(h))
        })
    }

    /// Retained-log hosts per RRID in `rrids`, under a single lock.
    ///
    /// The fan-out driver's one-shot snapshot, taken before the template loop so
    /// a `-t` fan-out pays one lock, not one per host.
    #[must_use]
    pub(crate) fn detached_snapshot(&self, rrids: &[String]) -> BTreeMap<String, BTreeSet<String>> {
        let logs = self.logs_or_recovered();
        let mut snapshot: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (rrid, host) in logs.keys() {
            if rrids.iter().any(|r| r == rrid) {
                snapshot
                    .entry(rrid.clone())
                    .or_default()
                    .insert(host.clone());
            }
        }
        snapshot
    }

    /// A `list_templates` row for `rrid`, or `None` if it is not loaded.
    ///
    /// Guard-aware, but a foreign holder yields [`TemplateRow::Busy`], not
    /// `None`, which would drop the template from the listing.
    #[must_use]
    pub(crate) fn template_row(&self, rrid: &str) -> Option<TemplateRow> {
        match self.with_report(rrid, |r| {
            TemplateRow::Read(r.base().targets.len(), r.base().workflow.as_str())
        }) {
            Ok(row) => Some(row),
            Err(ReportAccess::NotLoaded) => None,
            Err(ReportAccess::Busy) => Some(TemplateRow::Busy),
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

    /// The mutable counterpart of [`metadata`](Self::metadata). Never `None`.
    pub(crate) fn metadata_mut(&mut self) -> &mut (dyn TestReport + Send + Sync) {
        match &mut self.active_guard {
            Some(g) => (**g).as_mut(),
            None => self.null.as_mut(),
        }
    }

    /// Sets the active report's [`Workflow`] mode.
    ///
    /// Applies only while this session holds the active guard; otherwise a no-op
    /// returning `false`. That is wider than "no report loaded":
    /// [`release_active_guard`](Self::release_active_guard),
    /// [`take_teardown_units`](Self::take_teardown_units) and
    /// [`fork_for_call`](Self::fork_for_call) drop the guard with reports still
    /// registered, so a caller not behind `require_update` must honour the
    /// return value. Guardless, the write would land on the long-lived
    /// [`NullReport`] sentinel and outlive the load.
    pub(crate) fn set_workflow(&mut self, workflow: Workflow) -> bool {
        if self.active_guard.is_none() {
            tracing::debug!(
                ?workflow,
                "set_workflow with no active guard: ignoring, not mutating NullReport sentinel"
            );
            return false;
        }
        self.metadata_mut().base_mut().workflow = workflow;
        true
    }

    /// The active report's connected targets.
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.metadata().base().targets
    }

    /// The mutable counterpart of [`targets`](Self::targets).
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.metadata_mut().base_mut().targets
    }

    /// Moves the active report's targets out, leaving an empty group in place.
    ///
    /// The report's `perform_*` methods take `&self` **and** `&mut HostsGroup`,
    /// which one `&mut Box<dyn TestReport>` cannot hand out at once;
    /// [`restore_targets`](Self::restore_targets) puts the group back.
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
    /// A `Target` owns its connection, so a child group cannot borrow into the
    /// parent's; both halves come back and
    /// [`restore_split_targets`](Self::restore_split_targets) merges the
    /// remainder in afterwards.
    ///
    /// `hosts` is the parsed `-t` value: `None` (also `-t all`) selects every
    /// enabled host; `Some` names exactly those. `enabled` gates selection as for
    /// [`HostsGroup::select_split`](mtui_hosts::HostsGroup::select_split): a
    /// named-but-disabled host lands in the remainder, never dropped.
    ///
    /// # Errors
    ///
    /// [`mtui_hosts::HostError::NotConnected`] when a named `-t` host is not a
    /// member of the active report's group. The failed split consumes the taken
    /// group, leaving the report's empty.
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
    /// The counterpart to [`split_targets`](Self::split_targets).
    pub(crate) fn restore_split_targets(
        &mut self,
        mut selected: HostsGroup,
        remainder: HostsGroup,
    ) {
        selected.merge(remainder);
        self.restore_targets(selected);
    }

    /// Keeps the command logs of `group`'s hosts under `rrid` after the group
    /// is dropped, so `show_log` still answers for removed hosts.
    ///
    /// Called with the doomed group before it drops. An empty log drops its key
    /// instead of retaining, so a remove → re-add → remove cycle cannot resurrect
    /// a prior generation's protocol; a non-empty one overwrites. The lock is
    /// never held across an await.
    pub(crate) fn retain_detached_logs(&self, rrid: &str, group: &HostsGroup) {
        let mut keep = Vec::new();
        let mut drop_keys = Vec::new();
        for target in group.targets() {
            let name = target.hostname().to_owned();
            if target.out().is_empty() {
                drop_keys.push(name);
            } else {
                keep.push((name, Arc::new(target.out().clone())));
            }
        }
        let mut logs = self.logs_or_recovered();
        for (name, log) in keep {
            logs.insert((rrid.to_owned(), name), log);
        }
        for name in drop_keys {
            logs.remove(&(rrid.to_owned(), name));
        }
    }

    /// Names of removed hosts with a retained log under `rrid`, sorted.
    #[must_use]
    pub(crate) fn detached_hosts(&self, rrid: &str) -> Vec<String> {
        self.logs_or_recovered()
            .keys()
            .filter(|(r, _)| r == rrid)
            .map(|(_, h)| h.clone())
            .collect()
    }

    /// The retained log of removed host `host` under `rrid`, windowed to
    /// `[start, end)` with the pre-window total, so `--limit 0` clones no
    /// entries. An inverted window reads as empty.
    #[must_use]
    pub(crate) fn detached_window(
        &self,
        rrid: &str,
        host: &str,
        start: usize,
        end: usize,
    ) -> Option<(usize, Vec<LogEntry>)> {
        let log = self
            .logs_or_recovered()
            .get(&(rrid.to_owned(), host.to_owned()))
            .cloned()?;
        let total = log.len();
        let end = end.min(total);
        let start = start.min(total).min(end);
        let entries = log[start..end]
            .iter()
            .map(|c| {
                (
                    c.command.clone(),
                    c.stdout.clone(),
                    c.stderr.clone(),
                    i32::from(c.exitcode),
                )
            })
            .collect();
        Some((total, entries))
    }

    /// Drops every retained log under `rrid` (after `unload` or a reload).
    pub(crate) fn forget_detached_logs(&self, rrid: &str) {
        self.logs_or_recovered().retain(|(r, _), _| r != rrid);
    }

    /// The detached-log map, recovering from a poisoned mutex.
    fn logs_or_recovered(&self) -> std::sync::MutexGuard<'_, DetachedLogs> {
        self.detached_logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Loads a template into the registry and, when requested, connects its
    /// reference hosts.
    ///
    /// 1. [`make_testreport`] loads the report from its teregen document (or
    ///    returns a null report on failure, which [`TemplateRegistry::add`]
    ///    ignores).
    /// 2. The report is registered and, with a real RRID, made active;
    ///    re-loading an RRID replaces its stored report, siblings untouched.
    /// 3. If it asked for autoconnect
    ///    ([`TestReportBase::autoconnect_pending`](mtui_testreport::TestReportBase::autoconnect_pending)),
    ///    its reference hosts are connected here, so `mtui-testreport` never
    ///    depends on `mtui-hosts`/`mtui-datasources`. An unreachable host is
    ///    logged and skipped, never aborting the load.
    ///
    /// Returns the loaded report's RRID, empty when the load failed and the null
    /// report was substituted.
    ///
    /// `force_continue` is forwarded to [`make_testreport`]: the non-interactive
    /// escape hatch for a stale template hash.
    pub async fn load_update(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
        force_continue: bool,
    ) -> String {
        self.load_update_reported(update, autoconnect, kind, force_continue)
            .await
            .0
    }

    /// [`load_update`](Self::load_update) that also returns *why* a load failed.
    ///
    /// Returns `(rrid, load_error)`: on failure an empty RRID plus the
    /// diagnostic [`make_testreport`] stashed on the substituted null report.
    pub(crate) async fn load_update_reported(
        &mut self,
        update: &UpdateID,
        autoconnect: bool,
        kind: UpdateKind,
        force_continue: bool,
    ) -> (String, Option<String>) {
        let report = make_testreport(
            update,
            self.config.clone(),
            kind,
            autoconnect,
            self.is_repl,
            self.prompter.as_ref(),
            force_continue,
        )
        .await;
        let rrid = report.id();
        let pending = report.base().autoconnect_pending;
        let load_error = if rrid.is_empty() {
            report.base().load_error.clone()
        } else {
            None
        };

        // `add_or_replace` locks the old entry, which would self-deadlock against
        // a guard still held here (e.g. `regenerate`). Re-installed below.
        self.active_guard = None;

        let removed = self.templates.add_or_replace(report).await;
        for (host, err) in &removed.failed {
            tracing::warn!("failed to disconnect from {host} while reloading: {err}");
        }
        for host in &removed.stragglers {
            tracing::warn!("still disconnecting from {host} while reloading");
        }
        for line in crate::commands::support::leftover_notices(&removed.leftovers) {
            self.display.println(&line);
        }
        if !rrid.is_empty() {
            // A reload starts a fresh generation.
            self.forget_detached_logs(&rrid);
            self.templates.set_active(&rrid);
        }
        // Restores the guard released above, onto the new template or (on a
        // failed load) the prior active.
        self.refresh_active_guard();

        // A PI under `[lock] pi_autolock` is locked for the life of the report;
        // `lock_connected_target` locks each host as it arrives, so seeding the
        // comment before any host connects is all that is needed.
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
    /// Resolves the wanted hosts (the template's `reference host:` names plus one
    /// host per matching testplatform slot), then connects a [`Target`] for each,
    /// stamping the report's RRID as the pool-claim owner.
    async fn autoconnect_active(&mut self, rrid: &str) {
        // Snapshot synchronously: a `Session` borrow held across the await
        // would make this future non-`Send`.
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
        ref_hosts.sort();
        ref_hosts.dedup();

        // Testplatform hosts go through pool selection (one host per slot).
        let (wanted, _unserved) = self
            .resolve_and_record_pool(&config, ref_hosts, testplatforms, arbiter, owner, shuffle)
            .await;
        let wanted: Vec<String> = wanted
            .into_iter()
            .filter(|h| !already.contains(h))
            .collect();
        let pool_primaries: std::collections::HashSet<String> = wanted.iter().cloned().collect();

        let (unserved, failures) = self
            .connect_and_add_hosts(wanted, rrid, &pool_primaries)
            .await;
        // `load_template` prints no unserved-slot lines; name a refusal here.
        self.show_refused(&failures, &unserved);
    }

    /// Prints `skipped <host>: <reason>` for each host attach refused.
    fn show_refused(&mut self, failures: &[(String, HostFailure)], unserved: &[Unserved]) {
        for (host, failure) in refused_hosts(failures, unserved) {
            self.display.println(
                &self
                    .display
                    .yellow(&format!("skipped {host}: {}", failure.render())),
            );
        }
    }

    /// Builds a live [`Target`] for each host in `hosts`, connects it, and adds
    /// the ones that connect to the active report's group; connect failures are
    /// logged and skipped so one bad host never aborts the batch.
    ///
    /// The shared connect loop behind
    /// [`autoconnect_active`](Self::autoconnect_active) and `add_host`. Each
    /// target is stamped with `rrid` as its pool-claim owner.
    ///
    /// A freshly connected host is autolocked with the active report's
    /// `lock_comment` when a PI assignment is in progress; one already locked by
    /// another owner is left as-is ([`HostError::TargetLocked`] suppressed), and
    /// a failed autolock never drops an otherwise-good host. It is also checked
    /// for product drift against its `refhosts.yml` row
    /// ([`verify_target_products`](Self::verify_target_products)); drift is
    /// recorded in `product_warnings` and never drops the host.
    ///
    /// Returns the slots the backup step tried and could not serve, plus one
    /// `(host, reason)` per attempted host that never attached.
    async fn connect_and_add_hosts(
        &mut self,
        hosts: Vec<String>,
        rrid: &str,
        pool_primaries: &std::collections::HashSet<String>,
    ) -> (Vec<Unserved>, Vec<(String, HostFailure)>) {
        let config = self.config.clone();
        // Everything below is snapshotted before the connect loop so no borrow
        // is held across the `.await` (the future must stay `Send`).
        let lock_comment = self.metadata().base().lock_comment.clone();
        let package_meta = PackageMeta::of(self.metadata().base());
        // `None` (headless) leaves the timeout an immediate abort.
        let timeout_prompt = self.prompter.as_ref().map(Prompter::as_timeout_prompt);
        let prompter = self.prompter.clone();
        let holders = self.op_lock_holders.clone();
        let injected = self.injected_connections();
        // A host the update workflow cannot drive is refused only while an
        // update is loaded.
        let require_command_set = !self.metadata().id().is_empty();
        // One inventory for the batch; `None` disables the drift check.
        let store = Self::build_refhosts_store(&config).await;
        // Whether a host takes the remote pool lock or the normal autolock.
        let pool_claims = self.metadata().base().pool_claims.clone();
        // A host the template names is never a pool draw.
        let named = self.metadata().base().hostnames.clone();
        let store_ref = store.as_ref();
        let package_meta = &package_meta;
        let timeout_prompt = &timeout_prompt;
        let lock_comment = &lock_comment;
        let config_ref = &config;
        let pool_claims_ref = &pool_claims;
        let named_ref = &named;
        let holders_ref = &holders;
        let injected_ref = &injected;
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
                pool_claims_ref.contains(host)
                    && pool_primaries.contains(host)
                    && !named_ref.contains(host),
                require_command_set,
                holders_ref,
                injected_ref.get(host).map(|c| c.clone_box()),
            )
        });
        let bound = (config.max_parallel as usize).max(1);
        let connect_futs: Vec<_> = connect_futs.collect();
        let connected = Self::connect_bounded(connect_futs, bound).await;

        let mut drift: Vec<(String, Option<Vec<String>>)> = Vec::new();
        let targets = self.targets_mut();
        // A group built by a later `load_update` would start without a prompter.
        if let Some(prompter) = prompter {
            targets.set_prompter(prompter);
        }
        targets.set_op_lock_holders(holders.clone());
        let mut failures: std::collections::HashMap<String, HostFailure> =
            std::collections::HashMap::new();
        for result in connected {
            match result {
                Ok((target, drift_entry)) => {
                    targets.add(target);
                    drift.push(drift_entry);
                }
                Err((host, failure)) => {
                    failures.insert(host, failure);
                }
            }
        }

        let (backup_drift, unserved, refused_backups) = self
            .connect_pool_backups(
                &config,
                rrid,
                &hosts,
                failures.clone(),
                timeout_prompt.clone(),
                pool_primaries,
                &injected,
                require_command_set,
                HostArbiter::try_acquire,
            )
            .await;
        drift.extend(backup_drift);

        self.apply_product_warnings(drift);
        // Failures for hosts that never attached (a backup sibling may have
        // served the slot instead), plus refused backup siblings.
        let attached: std::collections::HashSet<String> =
            self.targets().names().into_iter().collect();
        let mut connect_failures: Vec<(String, HostFailure)> = failures
            .into_iter()
            .filter(|(host, _)| !attached.contains(host))
            .chain(refused_backups)
            .collect();
        connect_failures.sort_by(|a, b| a.0.cmp(&b.0));
        // A refused host's drift is shown, not recorded.
        for (host, failure) in refused_hosts(&connect_failures, &unserved) {
            if let HostFailure::Unusable {
                drift: Some(lines), ..
            } = failure
            {
                self.show_product_drift(host, lines);
            }
        }
        (unserved, connect_failures)
    }

    /// Drives `futs` with at most `bound` in flight (the `[connection]
    /// max_parallel` cap), refilling a slot as soon as any finishes. Outputs come
    /// back in input order, since callers fold them order-sensitively.
    ///
    /// Takes a `Vec`: a lazy borrowing iterator in this future defeats the `Send`
    /// check of `Command::call`.
    async fn connect_bounded<F: Future>(futs: Vec<F>, bound: usize) -> Vec<F::Output> {
        use futures::StreamExt;

        let mut indexed: Vec<_> = futures::stream::iter(futs)
            .enumerate()
            .map(|(idx, fut)| async move { (idx, fut.await) })
            .buffer_unordered(bound.max(1))
            .collect()
            .await;
        indexed.sort_unstable_by_key(|(idx, _)| *idx);
        indexed.into_iter().map(|(_, out)| out).collect()
    }

    /// Takes the pool claim and/or operation lock on an already-connected
    /// `target`. Fails when a pool claim was required but lost the remote race,
    /// meaning the caller should drop the host.
    ///
    /// The two branches are sequential, not exclusive: a non-empty `lock_comment`
    /// (a loaded Product Increment) takes the operation lock regardless of
    /// `is_pool_claim`, so a pool-selected host is PI-locked too.
    async fn lock_connected_target(
        target: &mut Target,
        host: &str,
        rrid: &str,
        lock_comment: &str,
        is_pool_claim: bool,
    ) -> Result<(), HostFailure> {
        if is_pool_claim {
            // Losing the remote race drops the host so a sibling can be tried
            // (`connect_pool_backups` releases the in-process claim).
            let comment = format!("mtui pool {rrid} [{rrid}]");
            match target.pool_claim(&comment).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(host = %host, "claimed in-process but busy remotely; skipping");
                    return Err(HostFailure::ClaimedBy(
                        target
                            .pool_claim_rrid()
                            .await
                            .ok()
                            .flatten()
                            .filter(|s| !s.is_empty()),
                    ));
                }
                Err(e) => {
                    warn!(host = %host, error = %e, "pool claim failed remotely; skipping");
                    return Err(HostFailure::ClaimFailed);
                }
            }
        }
        Self::autolock_target(target, lock_comment).await;
        Ok(())
    }

    /// Connects a single host, autolocks + package-seeds + drift-verifies it,
    /// and returns the live [`Target`] plus its drift entry, or the host with
    /// the reason it was dropped.
    ///
    /// Shared by the concurrent initial batch and the sequential backup fallback
    /// ([`connect_pool_backups`](Self::connect_pool_backups)); all inputs are
    /// plain data so the future stays `Send`. A `from_pool` host is refused under
    /// another owner's live operation lock.
    #[allow(clippy::too_many_arguments)]
    async fn connect_one(
        config: &Config,
        host: String,
        rrid: &str,
        timeout_prompt: &Option<mtui_hosts::TimeoutPrompt>,
        lock_comment: &str,
        package_meta: &PackageMeta,
        store: Option<&Refhosts>,
        is_pool_claim: bool,
        from_pool: bool,
        require_command_set: bool,
        holders: &OpLockHolders,
        injected: Option<Box<dyn Connection>>,
    ) -> Result<(Target, (String, Option<Vec<String>>)), (String, HostFailure)> {
        // Test seam: a registered mock skips the SSH dial.
        let mut target = if let Some(conn) = injected {
            Target::with_connection_and_config(config, host.clone(), TargetState::Enabled, conn)
        } else {
            Target::new(config, host.clone(), TargetState::Enabled)
        };
        target.set_rrid(rrid.to_owned());
        target.set_op_lock_holders(holders.clone());
        // Before connecting: `Target::connect` applies it to the transport.
        if let Some(tp) = timeout_prompt.as_ref() {
            target.set_timeout_prompt(tp.clone());
        }
        match target.connect().await {
            Ok(()) => {
                // Before any claim or lock is written, so a pool slot can still
                // reach a sibling.
                if require_command_set
                    && let Some(gap) = mtui_testreport::reports::missing_command_set(&target)
                {
                    warn!(host = %host, %gap, "skipping host");
                    let drift = Self::verify_target_products(store, &target);
                    return Err((host, HostFailure::Unusable { gap, drift }));
                }
                // A pool draw has siblings; a named host is attached regardless.
                if from_pool {
                    match target.foreign_operation_lock().await {
                        Ok(None) => {}
                        Ok(Some(owner)) => {
                            warn!(host = %host, by = %owner.by, since = %owner.since, "operation lock held elsewhere; skipping");
                            return Err((
                                host,
                                HostFailure::OperationLocked {
                                    by: owner.by,
                                    since: owner.since,
                                },
                            ));
                        }
                        Err(e) => {
                            warn!(host = %host, error = %e, "operation lock unreadable; keeping host");
                        }
                    }
                }
                if let Err(failure) = Self::lock_connected_target(
                    &mut target,
                    &host,
                    rrid,
                    lock_comment,
                    is_pool_claim,
                )
                .await
                {
                    return Err((host, failure));
                }
                package_meta.reseed(&mut target);
                target.query_versions().await;
                let drift = Self::verify_target_products(store, &target);
                Ok((target, (host, drift)))
            }
            Err(e) => {
                warn!(host = %host, "connect failed, skipping: {e}");
                Err((host, HostFailure::Unreachable))
            }
        }
    }

    /// Retries failed pool slots against their remaining free candidates.
    ///
    /// Only a slot whose pool-claimed primary failed to connect in *this* call
    /// (`attempted_initial` ∩ `pool_claims`) and that has no candidate attached
    /// is retried: drop the dead claim(s), then match the retried slots jointly
    /// to their free siblings ([`match_slots`]), connecting one matched sibling
    /// at a time and re-matching after each, until every slot is served or out
    /// of free siblings; a slot left without one waits (`acquire_any`) for a
    /// busy sibling. A slot with any attached host is served, and a slot nobody
    /// claimed in this call is left alone. A candidate
    /// [`pool_slots`](mtui_testreport::TestReportBase::pool_slots) records for
    /// another slot counts for that slot only. Any host that connects joins the
    /// active group and returns its drift entry. A no-op when pool selection is
    /// inactive or no slots are recorded; a connect failure releases the
    /// in-process claim and moves on.
    ///
    /// The whole phase shares one wall-clock budget (`4 * connect_timeout`), not
    /// a per-slot one, which would multiply by slot count and siblings. Once
    /// spent, remaining slots and siblings are abandoned with a `warn!`.
    ///
    /// `primary_failures` carries why each of `attempted_initial` was dropped.
    /// Every retried slot that ends without a host is returned as [`Unserved`],
    /// and every sibling attach refused ([`HostFailure::is_refusal`]) is returned
    /// with its reason. `injected` and `require_command_set` are as for
    /// [`connect_one`](Self::connect_one). Each matched free sibling is claimed
    /// through `claim`.
    #[allow(clippy::too_many_arguments)]
    async fn connect_pool_backups(
        &mut self,
        config: &Config,
        rrid: &str,
        attempted_initial: &[String],
        primary_failures: std::collections::HashMap<String, HostFailure>,
        timeout_prompt: Option<mtui_hosts::TimeoutPrompt>,
        pool_primaries: &std::collections::HashSet<String>,
        injected: &std::collections::HashMap<String, Box<dyn Connection>>,
        require_command_set: bool,
        claim: ClaimFn,
    ) -> (
        Vec<(String, Option<Vec<String>>)>,
        Vec<Unserved>,
        Vec<(String, HostFailure)>,
    ) {
        let (arbiter, owner, slot_candidates, slot_of, lock_comment, package_meta, named) = {
            let base = self.metadata().base();
            (
                base.arbiter,
                base.owner.clone(),
                base.slot_candidates.clone(),
                base.pool_slots.clone(),
                base.lock_comment.clone(),
                PackageMeta::of(base),
                base.hostnames.clone(),
            )
        };
        let (Some(arbiter), Some(owner)) = (arbiter, owner) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        if slot_candidates.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        let store = Self::build_refhosts_store(config).await;
        let holders = self.op_lock_holders.clone();

        let (wait, poll) = lock_wait_secs(config);

        let attached: std::collections::HashSet<String> =
            self.targets().names().into_iter().collect();
        // Pool-claimed primaries this call tried and lost: only the pool
        // selection's, never a claim taken outside it (a named host claims too).
        // `slot_candidates` is cumulative history, so the caller threads the set.
        let failed: std::collections::HashSet<String> = {
            let claims = &self.metadata().base().pool_claims;
            attempted_initial
                .iter()
                .filter(|h| {
                    claims.contains(*h) && !attached.contains(*h) && pool_primaries.contains(*h)
                })
                .cloned()
                .collect()
        };
        // A dead claim must not outlive this call: a held host is skipped by
        // every later pool selection.
        {
            let base = self.metadata_mut().base_mut();
            for h in &failed {
                base.pool_claims.remove(h);
                base.pool_slots.remove(h);
                arbiter.release(h, &owner);
            }
        }
        let mut attempted: std::collections::HashSet<String> =
            attempted_initial.iter().cloned().collect();
        let mut new_drift: Vec<(String, Option<Vec<String>>)> = Vec::new();
        let mut unserved: Vec<Unserved> = Vec::new();
        let mut refused: Vec<(String, HostFailure)> = Vec::new();

        // Bounds the phase regardless of report size; a late slot may starve.
        let started = Instant::now();
        let budget = Duration::from_secs(4 * config.connect_timeout);

        let mut slots: Vec<_> = slot_candidates.into_iter().collect();
        slots.sort();
        let mut retried = Vec::new();
        for (slot, candidates) in slots {
            // A candidate drawn for another slot neither serves nor fails this one.
            let own: Vec<&String> = candidates
                .iter()
                .filter(|c| slot_of.get(*c).is_none_or(|s| *s == slot))
                .collect();
            if own.iter().any(|c| attached.contains(*c)) || !own.iter().any(|c| failed.contains(*c))
            {
                continue;
            }
            let slot_failures: Vec<(String, HostFailure)> = candidates
                .iter()
                .filter_map(|c| primary_failures.get(c).map(|f| (c.clone(), f.clone())))
                .collect();
            retried.push((slot, candidates, slot_failures));
        }
        // `None` while a slot is still retried, then whether it was served.
        let mut outcome: Vec<Option<bool>> = vec![None; retried.len()];
        let mut lost: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut budget_spent = false;
        while outcome.contains(&None) {
            if started.elapsed() >= budget {
                budget_spent = true;
                break;
            }
            // Jointly, so no slot takes the only free sibling another slot has.
            let free: Vec<Vec<&String>> = retried
                .iter()
                .zip(&outcome)
                .map(|((slot, candidates, _), decided)| {
                    candidates
                        .iter()
                        .filter(|c| {
                            decided.is_none()
                                && slot_of.get(*c).is_none_or(|s| s == slot)
                                && !attempted.contains(*c)
                                && !lost.contains(*c)
                                && arbiter.owner_of(c).is_none_or(|o| o == owner)
                        })
                        .collect()
                })
                .collect();
            let matched = match_slots(&free, |_, _| false)
                .into_iter()
                .enumerate()
                .find_map(|(i, host)| host.map(|h| (i, h.clone())));
            let (i, chosen) = if let Some((i, host)) = matched {
                if !claim(arbiter, &host, &owner) {
                    lost.insert(host);
                    continue;
                }
                (i, host)
            } else {
                // No free sibling is left: wait for a busy one, a slot at a time.
                let Some(i) = outcome.iter().position(Option::is_none) else {
                    break;
                };
                let (slot, candidates, slot_failures) = &mut retried[i];
                let remaining: Vec<String> = candidates
                    .iter()
                    .filter(|c| {
                        slot_of.get(*c).is_none_or(|s| s == slot) && !attempted.contains(*c)
                    })
                    .cloned()
                    .collect();
                let Some(host) = arbiter.acquire_any(&remaining, &owner, wait, poll).await else {
                    for c in &remaining {
                        let by = arbiter.owner_of(c).map(|o| o.1);
                        slot_failures.push((c.clone(), HostFailure::ClaimedBy(by)));
                    }
                    outcome[i] = Some(false);
                    continue;
                };
                (i, host)
            };
            let slot = retried[i].0.clone();
            attempted.insert(chosen.clone());
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
                !named.contains(&chosen),
                require_command_set,
                &holders,
                injected.get(&chosen).map(|c| c.clone_box()),
            )
            .await
            {
                Ok((target, drift_entry)) => {
                    self.metadata_mut()
                        .base_mut()
                        .pool_slots
                        .insert(chosen.clone(), slot);
                    self.targets_mut().add(target);
                    new_drift.push(drift_entry);
                    outcome[i] = Some(true);
                }
                Err((host, failure)) => {
                    // A served or budget-cut slot drops its failures; keep
                    // a refusal apart so it is still named.
                    if failure.is_refusal() {
                        refused.push((host.clone(), failure.clone()));
                    }
                    retried[i].2.push((host, failure));
                    let base = self.metadata_mut().base_mut();
                    base.pool_claims.remove(&chosen);
                    arbiter.release(&chosen, &owner);
                }
            }
        }
        for ((slot, candidates, slot_failures), decided) in retried.into_iter().zip(outcome) {
            match decided {
                Some(true) => {}
                None if budget_spent => {
                    warn!(
                        slot = %slot,
                        "backup-refhost retry budget exhausted; giving up on this slot"
                    );
                    unserved.push(Unserved::BudgetSpent {
                        slot,
                        tried: slot_failures.into_iter().map(|(h, _)| h).collect(),
                    });
                }
                _ => {
                    warn!(
                        slot = %slot,
                        candidates = candidates.len(),
                        "no connectable pool host for slot (all candidates tried)"
                    );
                    unserved.push(Unserved::Unconnectable {
                        slot,
                        failures: slot_failures,
                    });
                }
            }
        }
        (new_drift, unserved, refused)
    }

    /// Compares a freshly connected `target`'s detected products against its
    /// `refhosts.yml` row, returning the per-host warning lines to record.
    ///
    /// `Some(lines)` when [`compare`] reports drift. `None` means "no drift,
    /// clear any stale entry": the store is unavailable, the host is absent from
    /// `refhosts.yml`, or the products match. The host is kept regardless.
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

    /// Prints `host`'s product drift as a yellow warning block.
    fn show_product_drift(&mut self, host: &str, lines: &[String]) {
        self.display.println(&self.display.yellow(&format!(
            "{host}: products differ from refhosts.yml metadata:"
        )));
        for line in lines {
            self.display
                .println(&self.display.yellow(&format!("  - {line}")));
        }
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
                self.show_product_drift(host, lines);
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
    /// A no-op when `lock_comment` is empty. A host already locked by another
    /// owner is left as-is ([`HostError::TargetLocked`] suppressed); any other
    /// lock error is logged but never propagated, so a failed autolock never
    /// drops an otherwise-good host.
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
    /// testplatform, deduplicated against the group. Returns every slot or
    /// testplatform that ended without a host.
    pub(crate) async fn add_testplatform_hosts(&mut self) -> Vec<Unserved> {
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
        // `ref_hosts` is empty: this path draws purely from the testplatforms.
        let (mut wanted, mut unserved) = self
            .resolve_and_record_pool(&config, Vec::new(), testplatforms, arbiter, owner, shuffle)
            .await;
        wanted.retain(|h| !already.contains(h));
        wanted.sort();
        wanted.dedup();

        let rrid = self.metadata().id();
        let pool_primaries: std::collections::HashSet<String> = wanted.iter().cloned().collect();
        let (more_unserved, failures) = self
            .connect_and_add_hosts(wanted, &rrid, &pool_primaries)
            .await;
        // Name each refused host whose reason no unserved line gives.
        let named: std::collections::HashSet<&str> = more_unserved
            .iter()
            .flat_map(Unserved::host_failures)
            .map(|(h, _)| h.as_str())
            .collect();
        let unnamed: Vec<_> = failures
            .iter()
            .filter(|(h, _)| !named.contains(h.as_str()))
            .cloned()
            .collect();
        self.show_refused(&unnamed, &[]);
        unserved.extend(more_unserved);
        unserved
    }

    /// Claims explicitly-named `hosts` in-process and records them as pool
    /// claims, refusing hosts another template holds with their holder named.
    /// Returns `(claiming, refused)`.
    ///
    /// A sync seam around [`HostArbiter::try_acquire`], testable without a connect.
    fn claim_named(
        arbiter: &HostArbiter,
        owner: &Owner,
        pool_claims: &mut std::collections::HashSet<String>,
        wanted: Vec<String>,
    ) -> (Vec<String>, Vec<(String, HostFailure)>) {
        let mut claiming = Vec::with_capacity(wanted.len());
        let mut refused = Vec::new();
        for host in wanted {
            if arbiter.try_acquire(&host, owner) {
                pool_claims.insert(host.clone());
                claiming.push(host);
            } else {
                // A separate lock: the holder may have changed, degrading to
                // `None` ("claimed remotely") rather than a wrong RRID.
                let holder = arbiter.owner_of(&host).map(|(_, rrid)| rrid);
                warn!(host = %host, "held by another template, skipping");
                refused.push((host, HostFailure::ClaimedBy(holder)));
            }
        }
        (claiming, refused)
    }

    /// Connects+adds the explicitly-named `hosts` to the active report's group.
    ///
    /// The `add_host`-with-`-t` path: each host is stamped with the active
    /// report's RRID and connected. One already in the group is warned about and
    /// skipped.
    ///
    /// Named hosts take the in-process pool claim like pool-selected ones, so a
    /// later pool selection cannot hand them out again, and a host claimed by
    /// another template is refused with its holder named. Returns one
    /// `(host, reason)` per named host that never attached.
    pub(crate) async fn add_named_hosts(
        &mut self,
        hosts: Vec<String>,
    ) -> Vec<(String, HostFailure)> {
        let mut wanted = Vec::with_capacity(hosts.len());
        for host in hosts {
            if self.metadata().base().targets.contains(&host) {
                warn!(host = %host, "already connected to {host}, skipping");
            } else {
                wanted.push(host);
            }
        }
        // Snapshotted before any `.await`; without an arbiter/owner pair named
        // hosts attach unclaimed.
        let arbitration = self
            .metadata()
            .base()
            .arbiter
            .zip(self.metadata().base().owner.clone());
        // A failed connect drops only what `claim_named` took below, never an
        // earlier pool selection's claims.
        let mut pre_claims = std::collections::HashSet::new();
        let mut pre_owned = std::collections::HashSet::new();
        let mut refused = Vec::new();
        if let Some((arbiter, owner)) = &arbitration {
            let base = self.metadata().base();
            pre_claims = base.pool_claims.clone();
            for host in &wanted {
                if arbiter.owner_of(host).as_ref() == Some(owner) {
                    pre_owned.insert(host.clone());
                }
            }
            let base = self.metadata_mut().base_mut();
            (wanted, refused) = Self::claim_named(arbiter, owner, &mut base.pool_claims, wanted);
        }
        let rrid = self.metadata().id();
        // Named hosts are not pool primaries: their failures must not back up
        // slots nobody asked about.
        let no_primaries = std::collections::HashSet::new();
        let (_, connect_failures) = self
            .connect_and_add_hosts(wanted.clone(), &rrid, &no_primaries)
            .await;
        refused.extend(connect_failures);
        // Hosts that never attached hold no remote claim and must not keep the
        // in-process one: the backup phase only releases recorded slots.
        if let Some((arbiter, owner)) = arbitration {
            let attached: std::collections::HashSet<String> =
                self.targets().names().into_iter().collect();
            let base = self.metadata_mut().base_mut();
            for host in wanted {
                if !attached.contains(&host) {
                    if !pre_claims.contains(&host) {
                        base.pool_claims.remove(&host);
                    }
                    if !pre_owned.contains(&host) {
                        arbiter.release(&host, &owner);
                    }
                }
            }
        }
        refused
    }

    /// Builds the refhosts inventory on demand from `config`, or `None` on any
    /// resolver failure, which degrades both callers (host selection and
    /// [`verify_target_products`](Self::verify_target_products)) to a no-op.
    /// Takes `&Config` so the caller's future stays `Send`.
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

    /// Picks one distinct free host per test-target slot via the arbiter, per
    /// testplatform.
    ///
    /// [`search_pool_by_query`](Refhosts::search_pool_by_query) groups candidates
    /// by their *requested* slot, so interchangeable hosts collapse to one slot.
    /// Each slot's candidates are shuffled (the [`ShuffleFn`] seam) and recorded
    /// for sibling fallback, a host fewer other slots match ahead of one they
    /// also need. Each host serves at most one slot per call, and each slot key
    /// is drawn once. Slots are matched to free hosts and to hosts this owner
    /// already holds, not serving another slot, so that as many as possible are
    /// served, earlier slots first and as many by held hosts as that allows, and each
    /// free match is claimed (a host claimed elsewhere meanwhile is matched
    /// around). A slot no such assignment serves waits up to `[lock] wait`
    /// seconds for a busy candidate, else is reported as [`Unserved::Busy`], or
    /// as [`Unserved::Shared`] when every candidate serves another slot.
    ///
    /// `served` maps each host to the slot key it was drawn for, as
    /// [`TestReportBase::pool_slots`](mtui_testreport::TestReportBase::pool_slots):
    /// a held host recorded for another slot serves that one, whatever order
    /// the slots come in, and each host this call draws or keeps is recorded.
    ///
    /// Returns `(chosen_hosts, slot_candidates, unserved)`, keyed as
    /// [`TestReportBase::slot_candidates`](mtui_testreport::TestReportBase::slot_candidates),
    /// for the caller to write onto the active report before connecting.
    ///
    /// Static, so the caller's future stays `Send`.
    #[allow(clippy::too_many_arguments)]
    async fn pool_select(
        store: &Refhosts,
        testplatforms: &[String],
        arbiter: &'static HostArbiter,
        owner: &Owner,
        served: &mut std::collections::HashMap<String, String>,
        wait: i64,
        poll: i64,
        shuffle: ShuffleFn,
    ) -> (
        Vec<String>,
        std::collections::HashMap<String, Vec<String>>,
        Vec<Unserved>,
    ) {
        Self::pool_select_claiming(
            store,
            testplatforms,
            arbiter,
            owner,
            served,
            wait,
            poll,
            shuffle,
            HostArbiter::try_acquire,
        )
        .await
    }

    /// [`pool_select`](Self::pool_select) with the claim of each matched free
    /// host made through `claim`, so a test can lose one to another owner.
    #[allow(clippy::too_many_arguments)]
    async fn pool_select_claiming(
        store: &Refhosts,
        testplatforms: &[String],
        arbiter: &'static HostArbiter,
        owner: &Owner,
        served: &mut std::collections::HashMap<String, String>,
        wait: i64,
        poll: i64,
        shuffle: ShuffleFn,
        claim: ClaimFn,
    ) -> (
        Vec<String>,
        std::collections::HashMap<String, Vec<String>>,
        Vec<Unserved>,
    ) {
        use std::collections::HashMap;
        let mut slot_candidates: HashMap<String, Vec<String>> = HashMap::new();
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut drawn: std::collections::HashSet<String> = std::collections::HashSet::new();

        let searched: Vec<_> = testplatforms
            .iter()
            .map(|tp| {
                let attrs = Attributes::from_testplatform(tp);
                let pairs = store.search_pool_by_query(&attrs);
                (tp, attrs, pairs)
            })
            .collect();
        let mut slots_of: HashMap<&str, std::collections::HashSet<String>> = HashMap::new();
        for (host, slot) in searched.iter().flat_map(|(_, _, pairs)| pairs) {
            slots_of
                .entry(host.name.as_str())
                .or_default()
                .insert(slot_key(slot));
        }
        let demand: HashMap<String, usize> = slots_of
            .into_iter()
            .map(|(host, slots)| (host.to_owned(), slots.len()))
            .collect();

        // Per testplatform in order; `None` marks a slot's place until it is decided.
        let mut report: Vec<Option<Unserved>> = Vec::new();
        let mut slots: Vec<(String, Vec<String>, usize)> = Vec::new();
        for (tp, attrs, pairs) in searched {
            if pairs.is_empty() {
                info!("autoconnect: nothing found for testplatform {tp:?}");
                report.push(Some(Unserved::NoRefhost { tp: tp.clone() }));
                continue;
            }
            // A pair exists only for an arch some host matched; name the rest.
            let covered: std::collections::HashSet<&str> =
                pairs.iter().map(|(_, slot)| slot.2.as_str()).collect();
            let mut uncovered: Vec<&str> = Vec::new();
            for arch in attrs.iter().map(Attributes::arch) {
                if !arch.is_empty() && !covered.contains(arch) && !uncovered.contains(&arch) {
                    uncovered.push(arch);
                    report.push(Some(Unserved::NoArchRefhost {
                        tp: tp.clone(),
                        arch: arch.to_owned(),
                    }));
                }
            }
            // IndexMap iterates in first-seen slot order.
            let mut by_slot: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
            for (host, slot) in pairs {
                by_slot.entry(slot_key(&slot)).or_default().push(host.name);
            }

            for (slot, mut candidates) in by_slot {
                if !drawn.insert(slot.clone()) {
                    continue;
                }
                // Spread load, then remember the order for backup fallback.
                shuffle(&mut candidates);
                // Stable, so the shuffle still decides among equally needed hosts.
                candidates.sort_by_key(|c| demand.get(c).copied().unwrap_or(0));
                slot_candidates.insert(slot.clone(), candidates.clone());
                report.push(None);
                slots.push((slot, candidates, report.len() - 1));
            }
        }

        let ours = |c: &String| arbiter.owner_of(c).as_ref() == Some(owner);
        let mut got: Vec<Option<String>> = vec![None; slots.len()];
        let mut done = vec![false; slots.len()];
        // A host claimed elsewhere between the match and its claim is matched around.
        let mut lost: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let free: Vec<Vec<&String>> = slots
                .iter()
                .zip(&done)
                .map(|((slot, candidates, _), &done)| {
                    // A host held for another slot is offered only to that slot.
                    let held = |c: &String| ours(c) && served.get(c).is_none_or(|s| s == slot);
                    candidates
                        .iter()
                        .filter(|c| {
                            !done
                                && !used.contains(*c)
                                && !lost.contains(*c)
                                && (held(c) || arbiter.owner_of(c).is_none())
                        })
                        .collect()
                })
                .collect();
            let mut retry = false;
            // Held first, so no extra host is claimed where one we hold serves.
            for (i, host) in match_slots(&free, |_, c| ours(c)).into_iter().enumerate() {
                let Some(host) = host else { continue };
                let held = ours(host);
                if held || claim(arbiter, host, owner) {
                    used.insert(host.clone());
                    served.insert(host.clone(), slots[i].0.clone());
                    if !held {
                        got[i] = Some(host.clone());
                    }
                    done[i] = true;
                } else {
                    lost.insert(host.clone());
                    retry = true;
                }
            }
            if !retry {
                break;
            }
        }

        // No assignment serves these: wait for a busy candidate.
        for (i, (slot, candidates, pos)) in slots.iter().enumerate() {
            if done[i] {
                continue;
            }
            let (shared, others): (Vec<String>, Vec<String>) = candidates
                .iter()
                .cloned()
                .partition(|c| used.contains(c) || ours(c));
            if others.is_empty() {
                warn!(slot = %slot, "every pool candidate already serves another slot");
                report[*pos] = Some(Unserved::Shared {
                    slot: slot.clone(),
                    hosts: shared,
                });
                continue;
            }
            match arbiter.acquire_any(&others, owner, wait, poll).await {
                Some(host) => {
                    used.insert(host.clone());
                    served.insert(host.clone(), slot.clone());
                    got[i] = Some(host);
                }
                None => {
                    warn!(
                        slot = %slot,
                        candidates = candidates.len(),
                        "no free pool host for slot (all candidates busy)"
                    );
                    let holders = others
                        .iter()
                        .filter_map(|c| arbiter.owner_of(c).map(|o| (c.clone(), o.1)))
                        .collect();
                    report[*pos] = Some(Unserved::Busy {
                        slot: slot.clone(),
                        holders,
                        shared,
                    });
                }
            }
        }
        let chosen = got.into_iter().flatten().collect();
        let unserved = report.into_iter().flatten().collect();
        (chosen, slot_candidates, unserved)
    }

    /// Combines `ref_hosts` with pool-selected testplatform hosts, records the
    /// pool claims and slot candidates on the active report, and returns the
    /// deduplicated hosts to connect plus what pool selection left unserved.
    ///
    /// Shared by [`autoconnect_active`](Self::autoconnect_active) and
    /// [`add_testplatform_hosts`](Self::add_testplatform_hosts); each testplatform
    /// contributes one host per slot (via [`pool_select`](Self::pool_select)).
    /// [`TemplateRegistry::add`](crate::template_registry::TemplateRegistry::add)
    /// always wires the arbiter/owner, so an unwired call resolves no testplatform
    /// hosts and warns rather than connecting every `search()` match.
    async fn resolve_and_record_pool(
        &mut self,
        config: &Config,
        ref_hosts: Vec<String>,
        testplatforms: Vec<String>,
        arbiter: Option<&'static HostArbiter>,
        owner: Option<Owner>,
        shuffle: ShuffleFn,
    ) -> (Vec<String>, Vec<Unserved>) {
        let mut wanted = ref_hosts;
        let mut unserved = Vec::new();

        let tp_hosts = match (arbiter, owner) {
            (Some(arbiter), Some(owner)) if !testplatforms.is_empty() => {
                if let Some(store) = Self::build_refhosts_store(config).await {
                    let (wait, poll) = lock_wait_secs(config);
                    let mut served = self.metadata().base().pool_slots.clone();
                    let (chosen, slot_candidates, not_served) = Self::pool_select(
                        &store,
                        &testplatforms,
                        arbiter,
                        &owner,
                        &mut served,
                        wait,
                        poll,
                        shuffle,
                    )
                    .await;
                    // On the report so `quit` can release them.
                    let base = self.metadata_mut().base_mut();
                    for host in &chosen {
                        base.pool_claims.insert(host.clone());
                    }
                    base.slot_candidates.extend(slot_candidates);
                    base.pool_slots = served;
                    unserved = not_served;
                    chosen
                } else {
                    unserved.push(Unserved::InventoryUnavailable);
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
        (wanted, unserved)
    }

    /// Requests that the REPL loop exit after the current dispatch; read via
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
    pub fn set_log_level_sink(&mut self, sink: LogLevelSink) {
        self.log_level_sink = Some(sink);
    }

    /// Applies `level` through the installed sink, if any.
    ///
    /// Returns `true` when a sink was invoked; `false` when none is installed.
    pub(crate) fn apply_log_level(&mut self, level: LogLevel) -> bool {
        if let Some(sink) = self.log_level_sink.as_mut() {
            sink(level);
            true
        } else {
            false
        }
    }

    /// Installs the callback `notify_user` uses to surface a desktop
    /// notification; unset, notifications are a silent no-op.
    pub fn set_notify_sink(&mut self, sink: NotifySink) {
        self.notify_sink = Some(sink);
    }

    /// Surfaces a best-effort desktop notification through the installed sink;
    /// `error` selects the error-class toast. Returns `true` when a sink was
    /// invoked.
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
    /// The REPL wires a [`Prompter::stdin`](mtui_hosts::Prompter::stdin)-backed
    /// one here; `mtui-mcp` leaves it unset. It also reaches the active report's
    /// [`HostsGroup`], so connected hosts pick up the command-timeout prompt
    /// immediately; later connects inherit it via `connect_and_add_hosts`.
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

    /// Resolves the inventory on demand and searches it per testplatform; a
    /// resolver failure degrades to an empty result.
    ///
    /// Production dispatch always has the arbiter/owner wired (see
    /// [`Session::resolve_and_record_pool`]), so this survives only as the
    /// offline baseline [`autoconnect_hosts_of`] tests pool selection against.
    async fn resolve_testplatform_hosts(config: &Config, testplatforms: &[String]) -> Vec<String> {
        if testplatforms.is_empty() {
            return Vec::new();
        }

        let Some(store) = Session::build_refhosts_store(config).await else {
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

    #[test]
    fn lock_wait_secs_orders_wait_before_poll_and_saturates() {
        let mut c = config();
        c.lock_wait = 7;
        c.lock_wait_poll = 3;
        assert_eq!(lock_wait_secs(&c), (7, 3));
        c.lock_wait = u64::MAX;
        assert_eq!(lock_wait_secs(&c), (i64::MAX, 3));
    }

    #[test]
    fn fresh_session_active_is_null_and_unloaded() {
        let s = Session::new(config(), true);
        assert!(!s.metadata().is_loaded());
        assert!(s.templates.is_empty());
        assert_eq!(s.metadata().id(), "");
    }

    #[test]
    fn teregen_token_store_stays_out_of_the_real_data_dir() {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let s = Session::new(config(), false);
        let path = s
            .teregen_token_store()
            .expect("test store")
            .path()
            .to_owned();
        if let Some(data) = mtui_config::data_dir() {
            assert!(!path.starts_with(&data), "{path:?} is under {data:?}");
        }

        let fork = s.fork_for_call(CommandPromptDisplay::with_sink(
            Box::new(Vec::new()),
            ColorMode::Never,
        ));
        assert_eq!(fork.teregen_token_store().unwrap().path(), path);
        assert_ne!(
            Session::new(config(), false)
                .teregen_token_store()
                .unwrap()
                .path(),
            path,
            "each session gets its own store"
        );
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

    #[tokio::test]
    async fn with_report_of_an_unloaded_rrid_is_not_loaded() {
        let mut s = Session::new(config(), false);
        assert_eq!(
            s.with_report("SUSE:Maintenance:9:9", |_| ()),
            Err(ReportAccess::NotLoaded)
        );
        assert_eq!(
            s.with_report_mut("SUSE:Maintenance:9:9", |_| ()),
            Err(ReportAccess::NotLoaded)
        );
    }

    #[tokio::test]
    async fn with_report_on_a_foreign_held_entry_is_busy_not_the_null_report() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        seed_report_with_host(&mut s, "SUSE:Maintenance:2:2", "t2");
        assert!(s.activate("SUSE:Maintenance:1:1").is_active());
        let _foreign = s
            .templates
            .handle("SUSE:Maintenance:2:2")
            .expect("seeded")
            .try_lock_owned()
            .expect("uncontended until now");

        assert_eq!(
            s.with_report("SUSE:Maintenance:2:2", |_| ()),
            Err(ReportAccess::Busy)
        );
        assert_eq!(
            s.with_report_mut("SUSE:Maintenance:2:2", |r| {
                r.base_mut().document_dirty = true;
            }),
            Err(ReportAccess::Busy)
        );
        assert!(
            !s.metadata().base().document_dirty,
            "a refused write must not land on the active report or the sentinel"
        );
    }

    #[tokio::test]
    async fn with_report_mut_reaches_the_guarded_active_report() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        assert!(s.activate("SUSE:Maintenance:1:1").is_active());

        s.with_report_mut("SUSE:Maintenance:1:1", |r| {
            r.base_mut().document_dirty = true;
        })
        .expect("the active entry is held by this session's own guard");
        assert_eq!(
            s.with_report("SUSE:Maintenance:1:1", |r| r.base().document_dirty),
            Ok(true)
        );

        s.release_active_guard();
        assert_eq!(
            s.with_report("SUSE:Maintenance:1:1", |r| r.base().document_dirty),
            Ok(true),
            "the write landed on the registry entry, not a copy"
        );
    }

    #[tokio::test]
    async fn with_report_reads_an_unheld_inactive_entry() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        seed_report_with_host(&mut s, "SUSE:Maintenance:2:2", "t2");
        assert!(s.activate("SUSE:Maintenance:1:1").is_active());

        assert_eq!(
            s.with_report("SUSE:Maintenance:2:2", |r| r.base().targets.names()),
            Ok(vec!["t2".to_owned()])
        );
    }

    /// The guard-aware probes the fan-out driver and `list_templates` read,
    /// across the four states an entry can be in. A contended entry must never
    /// read as absent (#524).
    #[tokio::test]
    async fn report_probes_distinguish_absent_active_unheld_and_foreign_held() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        seed_report_with_host(&mut s, "SUSE:Maintenance:2:2", "t2");
        seed_report_with_host(&mut s, "SUSE:Maintenance:3:3", "t3");
        assert!(s.activate("SUSE:Maintenance:1:1").is_active());
        s.with_report_mut("SUSE:Maintenance:2:2", |r| {
            r.base_mut().document_dirty = true
        })
        .expect("unheld");
        let foreign = "SUSE:Maintenance:3:3";
        let _held = s
            .templates
            .handle(foreign)
            .expect("seeded")
            .try_lock_owned()
            .expect("uncontended until now");
        let named = |h: &str| vec![h.to_owned()];

        // Absent: nothing to act on, nothing to lose.
        let absent = "SUSE:Maintenance:9:9";
        assert_eq!(s.is_hostless(absent), Some(true));
        assert_eq!(s.document_dirty(absent), Some(false));
        assert_eq!(s.owns_none_of(absent, &named("t1")), Some(true));
        assert_eq!(s.template_row(absent), None);

        // Active (read through this session's own guard) and unheld inactive.
        for (rrid, host, dirty) in [
            ("SUSE:Maintenance:1:1", "t1", false),
            ("SUSE:Maintenance:2:2", "t2", true),
        ] {
            assert_eq!(s.is_hostless(rrid), Some(false), "{rrid}");
            assert_eq!(s.document_dirty(rrid), Some(dirty), "{rrid}");
            assert_eq!(s.owns_none_of(rrid, &named(host)), Some(false), "{rrid}");
            assert_eq!(s.owns_none_of(rrid, &named("zz")), Some(true), "{rrid}");
            assert_eq!(
                s.template_row(rrid),
                Some(TemplateRow::Read(1, s.metadata().base().workflow.as_str())),
                "{rrid}"
            );
        }

        // Held by a foreign dispatch: unreadable, not absent.
        assert_eq!(s.is_hostless(foreign), None);
        assert_eq!(s.document_dirty(foreign), None);
        assert_eq!(s.owns_none_of(foreign, &named("t3")), None);
        assert_eq!(s.template_row(foreign), Some(TemplateRow::Busy));
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
        assert!(
            s.activate("SUSE:Maintenance:1:1").is_active(),
            "seeded template must activate"
        );

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
    use mtui_types::hostlog::CommandLog;

    /// A config whose refhosts resolver is the offline file-backed `path`
    /// resolver pointed at a live copy of the `mtui_types::fixtures` inventory
    /// (no network). The compiled constant is not a file, so each call
    /// materializes it into a fresh temp dir; the returned dir must outlive
    /// the session under test.
    fn config_with_path_refhosts() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("refhosts.yml"),
            mtui_types::fixtures::REFHOSTS_YML,
        )
        .unwrap();
        let mut c = Config::default();
        c.refhosts_resolvers = "path".to_owned();
        c.refhosts_path = dir.path().join("refhosts.yml");
        (dir, c)
    }

    /// A teregen serving `rrid`'s document, and `config` pointed at it.
    async fn teregen_serving(config: &mut Config, rrid: &str, kind: &str) -> wiremock::MockServer {
        use crate::commands::testkit::teregen::{document_json, mount_document, mount_schema};
        let server = wiremock::MockServer::start().await;
        mount_document(&server, rrid, &document_json(rrid, kind)).await;
        mount_schema(&server).await;
        config.teregen_api_v2 = server.uri();
        server
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
        assert!(
            session.activate(rrid).is_active(),
            "seeded template must activate"
        );
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
        for host in resolve_testplatform_hosts(&config, &testplatforms).await {
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
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
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
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &["only.example.com"], &[]);

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["only.example.com".to_owned()]);
    }

    /// A testplatform matching nothing in the inventory contributes no hosts;
    /// the reference hosts still stand.
    #[tokio::test]
    async fn autoconnect_hosts_unmatched_testplatform_yields_reference_only() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(
            &mut s,
            "SUSE:Maintenance:1:1",
            &["ref-only.example.com"],
            &["base=sles(major=99,minor=sp9);arch=[nonesuch]"],
        );

        let hosts = autoconnect_hosts_of(&s).await;
        assert_eq!(hosts, vec!["ref-only.example.com".to_owned()]);
    }

    /// A kernel update loads the report and activates it without
    /// autoconnecting, so a load never touches a live host.
    #[tokio::test]
    async fn load_update_kernel_loads_and_activates_without_connect() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let _teregen = teregen_serving(&mut config, rrid, "maintenance").await;
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        let loaded = s
            .load_update(&update, true, UpdateKind::Kernel, false)
            .await;

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

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let _teregen = teregen_serving(&mut config, rrid, "pi").await;
        assert!(config.lock_pi_autolock, "default must be enabled");
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel, false)
            .await;

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

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let _teregen = teregen_serving(&mut config, rrid, "maintenance").await;
        assert!(config.lock_pi_autolock, "default must be enabled");
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel, false)
            .await;

        assert_eq!(s.metadata().base().lock_comment, "");
    }

    /// `lock_pi_autolock = false` leaves the PI's `lock_comment` empty.
    #[tokio::test]
    async fn load_update_reported_leaves_lock_comment_empty_when_autolock_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:PI:1.2:5";

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let _teregen = teregen_serving(&mut config, rrid, "pi").await;
        config.lock_pi_autolock = false;
        let mut s = Session::new(config, false);

        let update = UpdateID::parse(rrid).unwrap();
        s.load_update(&update, true, UpdateKind::Kernel, false)
            .await;

        assert_eq!(s.metadata().base().lock_comment, "");
    }

    /// A reload replacing a loaded template closes its hosts, so it warns
    /// about what they were left with like `unload` does.
    #[tokio::test]
    async fn load_update_reload_names_a_lock_left_on_a_replaced_host() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        use mtui_hosts::{MockConnection, Target};
        use mtui_types::enums::TargetState;

        let tmp = tempfile::tempdir().unwrap();
        let rrid = "SUSE:Maintenance:24993:275518";
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let _teregen = teregen_serving(&mut config, rrid, "maintenance").await;
        let buf = crate::commands::testkit::Buffer::new();
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut s = Session::with_display(config, false, display);
        seed_active_report(&mut s, rrid, &[], &[]);
        let mut down = Target::with_connection(
            "h1",
            TargetState::Enabled,
            Box::new(MockConnection::new("h1").inactive()),
        );
        down.lock("").await.expect("operation lock taken");
        s.targets_mut().add(down);

        let update = UpdateID::parse(rrid).unwrap();
        let loaded = s
            .load_update(&update, true, UpdateKind::Kernel, false)
            .await;

        assert_eq!(loaded, rrid);
        assert!(
            buf.contents().contains(
                "warning: h1: operation lock not released (link down); unless the host \
                 reboots, it stays until it is older than [lock] stale_age or someone runs \
                 `unlock -f`"
            ),
            "{}",
            buf.contents()
        );
    }

    /// An unloadable RRID falls back to the null report: nothing registered,
    /// empty RRID returned.
    #[tokio::test]
    async fn load_update_missing_report_returns_empty_and_registers_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.template_dir = tmp.path().to_path_buf();
        let teregen = wiremock::MockServer::start().await;
        config.teregen_api_v2 = teregen.uri();
        let mut s = Session::new(config, false);

        let update = UpdateID::parse("SUSE:Maintenance:1:1").unwrap();
        let (loaded, reason) = s
            .load_update_reported(&update, true, UpdateKind::Auto, false)
            .await;

        assert_eq!(loaded, "");
        assert!(s.templates.is_empty());
        let reason = reason.expect("a failed load should report a reason");
        assert!(
            reason.contains("no document yet"),
            "reason should name the underlying cause: {reason}"
        );
    }

    /// `set_workflow` mutates the active report's workflow mode.
    #[test]
    fn set_workflow_mutates_active_report() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        assert_eq!(s.metadata().workflow(), Workflow::Manual);
        s.set_workflow(Workflow::Auto);
        assert_eq!(s.metadata().workflow(), Workflow::Auto);
    }

    /// With no guard held `set_workflow` must leave the `NullReport` sentinel
    /// alone and report `false`; with one it writes through (#484).
    #[test]
    fn set_workflow_with_no_active_report_does_not_mutate_sentinel() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        // No active report => null sentinel, default Manual.
        assert_eq!(s.metadata().workflow(), Workflow::Manual);
        // Attempt to set Auto with no active report must be a no-op.
        assert!(!s.set_workflow(Workflow::Auto));
        assert_eq!(
            s.metadata().workflow(),
            Workflow::Manual,
            "sentinel must not be mutated"
        );
        // Seed the sentinel off its default first, so the next no-op is a value
        // the write would visibly overwrite rather than Manual-over-Manual.
        s.metadata_mut().base_mut().workflow = Workflow::Auto;
        assert!(!s.set_workflow(Workflow::Manual));
        assert_eq!(
            s.metadata().workflow(),
            Workflow::Auto,
            "sentinel must not be mutated"
        );
        // With an active report, the mutation does apply.
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        assert_eq!(s.metadata().workflow(), Workflow::Manual, "precondition");
        assert!(s.set_workflow(Workflow::Auto));
        assert_eq!(s.metadata().workflow(), Workflow::Auto);
    }

    /// Connect seeds a host from the report targets matching its products: a
    /// plain-SLES base carrying the LTSS addon tracks the packages of a report
    /// whose only target is the LTSS product.
    #[tokio::test]
    async fn add_named_hosts_seeds_packages_from_the_hosts_matching_products() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let doc: mtui_types::report_document::ReportDocument =
            mtui_types::fixtures::MAINTENANCE_OBS.parse().unwrap();
        let mut ingested = mtui_testreport::TestReportBase::new(s.config.clone());
        mtui_testreport::apply_document(&mut ingested, &doc);
        {
            let base = s.metadata_mut().base_mut();
            base.packages = ingested.packages;
            base.host_packages = ingested.host_packages;
        }
        let product = |name: &str| {
            format!(
                "<product><name>{name}</name><baseversion>15</baseversion>\
                 <patchlevel>6</patchlevel><arch>s390x</arch></product>"
            )
            .into_bytes()
        };
        s.inject_mock_connection(
            "ltss.invalid",
            MockConnection::new("ltss.invalid")
                .with_listing("/etc/products.d", ["SLES.prod", "SLES-LTSS.prod"])
                .with_link("/etc/products.d/baseproduct", "SLES.prod")
                .with_file("/etc/products.d/SLES.prod", product("SLES"))
                .with_file("/etc/products.d/SLES-LTSS.prod", product("SLES-LTSS")),
        );

        let refused = s.add_named_hosts(vec!["ltss.invalid".to_owned()]).await;

        assert!(refused.is_empty(), "{refused:?}");
        let target = s.targets().get("ltss.invalid").expect("attached");
        assert_eq!(target.system().get_base().version, "15-SP6");
        let names: Vec<&str> = target.packages().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["smc-tools", "smc-tools-completion"]);
    }

    /// A report whose version-keyed map alone is filled, even under the
    /// product-agnostic `"standard"` key, seeds no host.
    #[tokio::test]
    async fn add_named_hosts_seeds_nothing_from_the_version_map_alone() {
        use std::collections::HashMap;

        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        s.metadata_mut().base_mut().packages = HashMap::from([(
            "standard".to_owned(),
            HashMap::from([("examplepkg".to_owned(), "1.0-1.1".to_owned())]),
        )]);
        s.inject_mock_connection(
            "versionmap.invalid",
            MockConnection::new("versionmap.invalid")
                .with_listing("/etc/products.d", ["SLES.prod"])
                .with_link("/etc/products.d/baseproduct", "SLES.prod")
                .with_file(
                    "/etc/products.d/SLES.prod",
                    b"<product><name>SLES</name><baseversion>15</baseversion>\
                      <patchlevel>6</patchlevel><arch>x86_64</arch></product>"
                        .to_vec(),
                ),
        );

        let refused = s
            .add_named_hosts(vec!["versionmap.invalid".to_owned()])
            .await;

        assert!(refused.is_empty(), "{refused:?}");
        let target = s.targets().get("versionmap.invalid").expect("attached");
        assert!(target.packages().is_empty(), "{:?}", target.packages());
    }

    /// An unreachable host fails its live connect and is skipped, not added.
    #[tokio::test]
    async fn add_named_hosts_skips_unconnectable() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
        }
        s.add_named_hosts(vec!["unreachable.invalid".to_owned()])
            .await;
        assert!(s.targets().is_empty());
    }

    /// A host already in the active group is warned about and dropped before the
    /// connect loop, leaving the group size unchanged.
    #[tokio::test]
    async fn add_named_hosts_skips_already_connected() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
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

    /// `claim_named` records free hosts as pool claims and refuses foreign-held
    /// ones with their holder named.
    #[test]
    fn claim_named_claims_free_and_refuses_held() {
        use mtui_hosts::HostArbiter;

        let arbiter = HostArbiter::new();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("held", &other));
        let mut claims = std::collections::HashSet::new();

        let (claiming, refused) = Session::claim_named(
            &arbiter,
            &owner,
            &mut claims,
            vec!["free".to_owned(), "held".to_owned()],
        );

        assert_eq!(claiming, ["free"]);
        assert!(claims.contains("free"));
        assert!(!claims.contains("held"));
        assert_eq!(arbiter.owner_of("free"), Some(owner));
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, "held");
        assert!(
            matches!(&refused[0].1, HostFailure::ClaimedBy(Some(rrid)) if rrid == "SUSE:Maintenance:2:2"),
            "{:?}",
            refused[0].1
        );
    }

    /// Claiming is idempotent for hosts this owner already holds.
    #[test]
    fn claim_named_keeps_own_holds() {
        use mtui_hosts::HostArbiter;

        let arbiter = HostArbiter::new();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        assert!(arbiter.try_acquire("mine", &owner));
        let mut claims = std::collections::HashSet::from(["mine".to_owned()]);

        let (claiming, refused) =
            Session::claim_named(&arbiter, &owner, &mut claims, vec!["mine".to_owned()]);

        assert_eq!(claiming, ["mine"]);
        assert!(refused.is_empty());
    }

    /// A named host held by another template is refused before any connect:
    /// nothing attaches, nothing is claimed, and the holder is named.
    #[tokio::test]
    async fn add_named_hosts_refuses_foreign_held_host() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner);
        }
        assert!(arbiter.try_acquire("h1", &other));

        let refused = s.add_named_hosts(vec!["h1".to_owned()]).await;

        assert!(s.targets().is_empty());
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, "h1");
        assert!(
            matches!(&refused[0].1, HostFailure::ClaimedBy(Some(rrid)) if rrid == "SUSE:Maintenance:2:2"),
            "{:?}",
            refused[0].1
        );
        assert!(!s.metadata().base().pool_claims.contains("h1"));
        assert_eq!(arbiter.owner_of("h1"), Some(other));
    }

    /// A named host that fails to connect keeps neither its pool claim nor
    /// its in-process claim, and its failure comes back with its reason.
    #[tokio::test]
    async fn add_named_hosts_releases_failed_claims() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
        }

        let refused = s
            .add_named_hosts(vec!["unreachable.invalid".to_owned()])
            .await;

        assert_eq!(
            refused,
            vec![("unreachable.invalid".to_owned(), HostFailure::Unreachable)]
        );
        assert!(s.targets().is_empty());
        assert!(
            !s.metadata()
                .base()
                .pool_claims
                .contains("unreachable.invalid")
        );
        assert_eq!(arbiter.owner_of("unreachable.invalid"), None);
    }

    /// A named host whose remote pool claim another RRID holds is refused
    /// with its holder named instead of vanishing into a plain skip.
    #[tokio::test]
    async fn add_named_hosts_names_remote_holder() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let foreign =
            format!("{now}:otheruser:99:mtui pool SUSE:Maintenance:2:2 [bob]").into_bytes();
        s.inject_mock_connection(
            "remote-held.invalid",
            crate::commands::testkit::product_host("remote-held.invalid", "SLES", "15", "6", false)
                .with_file(mtui_hosts::POOL_LOCK_PATH, foreign),
        );

        let refused = s
            .add_named_hosts(vec!["remote-held.invalid".to_owned()])
            .await;

        assert!(s.targets().is_empty());
        assert_eq!(
            refused,
            vec![(
                "remote-held.invalid".to_owned(),
                HostFailure::ClaimedBy(Some("SUSE:Maintenance:2:2".to_owned())),
            )]
        );
        assert!(
            !s.metadata()
                .base()
                .pool_claims
                .contains("remote-held.invalid")
        );
        assert_eq!(arbiter.owner_of("remote-held.invalid"), None);
    }

    /// A failed connect drops only the claims this call took: a claim the
    /// owner held before the call (e.g. from an earlier pool selection) stays.
    #[tokio::test]
    async fn add_named_hosts_keeps_preexisting_claim_on_failure() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        assert!(arbiter.try_acquire("preheld.invalid", &owner));
        {
            let base = s.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
            base.pool_claims.insert("preheld.invalid".to_owned());
        }

        let refused = s.add_named_hosts(vec!["preheld.invalid".to_owned()]).await;

        assert!(s.targets().is_empty());
        assert_eq!(
            refused,
            vec![("preheld.invalid".to_owned(), HostFailure::Unreachable)]
        );
        assert!(
            s.metadata().base().pool_claims.contains("preheld.invalid"),
            "a claim held before the call must survive its failed connect"
        );
        assert_eq!(arbiter.owner_of("preheld.invalid"), Some(owner));
    }

    /// Testplatforms resolve via the offline `path` resolver and are then
    /// connected; the unreachable fixture hosts are skipped, but the resolution
    /// path is exercised.
    #[tokio::test]
    async fn add_testplatform_hosts_resolves_and_connects() {
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
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
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
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
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, true);
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
        crate::commands::testkit::target_with(host, |c| c)
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
        assert_eq!(ok, Ok(()), "a free host must be claimable");
        assert!(
            t.is_locked().await.expect("is_locked"),
            "a pool-claimed host must also hold the PI operation lock"
        );
    }

    /// Losing the remote claim race names the RRID that holds it.
    #[tokio::test]
    async fn lock_connected_target_names_the_remote_claimant() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let foreign =
            format!("{now}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes();
        let conn = MockConnection::new("h1").with_file(mtui_hosts::POOL_LOCK_PATH, foreign);
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_rrid("SUSE:Maintenance:1:1");

        let res =
            Session::lock_connected_target(&mut t, "h1", "SUSE:Maintenance:1:1", "", true).await;

        assert_eq!(
            res,
            Err(HostFailure::ClaimedBy(Some(
                "SUSE:Maintenance:9:9".to_owned()
            )))
        );
    }

    /// A remote claim whose comment is not a pool stamp names no RRID, so the
    /// refusal degrades to "claimed remotely" rather than "claimed by ".
    #[tokio::test]
    async fn lock_connected_target_empty_holder_degrades_to_claimed_remotely() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let foreign = format!("{now}:otheruser:99:manual reservation").into_bytes();
        let conn = MockConnection::new("h1").with_file(mtui_hosts::POOL_LOCK_PATH, foreign);
        let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        t.set_rrid("SUSE:Maintenance:1:1");

        let res =
            Session::lock_connected_target(&mut t, "h1", "SUSE:Maintenance:1:1", "", true).await;

        assert_eq!(res, Err(HostFailure::ClaimedBy(None)));
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
        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
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

        let (chosen, slot_candidates, unserved) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(
            chosen.len(),
            2,
            "expected one host per slot, got {chosen:?}"
        );
        assert_eq!(unserved, Vec::new(), "every requested arch is covered");
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

        let (chosen, _, _) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

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

        let (chosen, slot_candidates, unserved) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &mine,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert!(chosen.is_empty(), "all candidates busy → no claim");
        // Candidates are still recorded (for backup once one frees up).
        assert_eq!(slot_candidates.len(), 1);
        let slot = slot_candidates.keys().next().unwrap().clone();
        assert_eq!(
            unserved,
            vec![Unserved::Busy {
                slot,
                holders: vec![
                    ("x86-a".to_owned(), "SUSE:Maintenance:2:2".to_owned()),
                    ("x86-b".to_owned(), "SUSE:Maintenance:2:2".to_owned()),
                ],
                shared: Vec::new(),
            }]
        );
    }

    /// A testplatform no refhost matches is named, not just logged.
    #[tokio::test]
    async fn pool_select_names_testplatform_without_refhost() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64")]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=12,minor=5);arch=[x86_64]".to_owned()];

        let (chosen, slot_candidates, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert!(chosen.is_empty() && slot_candidates.is_empty());
        assert_eq!(unserved, vec![Unserved::NoRefhost { tp: tps[0].clone() }]);
    }

    /// An arch of a matched testplatform that no host covers is named too.
    #[tokio::test]
    async fn pool_select_names_arch_without_refhost() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64")]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64,s390x]".to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["x86-a".to_owned()]);
        assert_eq!(
            unserved,
            vec![Unserved::NoArchRefhost {
                tp: tps[0].clone(),
                arch: "s390x".to_owned(),
            }]
        );
    }

    /// A repeated arch is reported once.
    #[tokio::test]
    async fn pool_select_names_repeated_uncovered_arch_once() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64")]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64,s390x,s390x]".to_owned()];

        let (_, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(
            unserved,
            vec![Unserved::NoArchRefhost {
                tp: tps[0].clone(),
                arch: "s390x".to_owned(),
            }]
        );
    }

    /// `arch=[]` requests any arch, so no arch can be left uncovered.
    #[tokio::test]
    async fn pool_select_any_arch_testplatform_names_no_arch() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64")]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec!["base=sles(major=15,minor=5);arch=[]".to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["x86-a".to_owned()]);
        assert_eq!(unserved, Vec::new());
    }

    /// Per testplatform, in testplatform order: its uncovered arches in
    /// requested order, then its busy slots.
    #[tokio::test]
    async fn pool_select_orders_uncovered_arches_per_testplatform() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64")]);
        let arbiter = test_arbiter();
        let mine: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("x86-a", &other));
        let tps = vec![
            "base=sles(major=15,minor=5);arch=[s390x,x86_64,aarch64]".to_owned(),
            "base=sles(major=12,minor=5);arch=[x86_64]".to_owned(),
        ];

        let (_, slot_candidates, unserved) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &mine,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        let slot = slot_candidates.keys().next().unwrap().clone();
        assert_eq!(
            unserved,
            vec![
                Unserved::NoArchRefhost {
                    tp: tps[0].clone(),
                    arch: "s390x".to_owned(),
                },
                Unserved::NoArchRefhost {
                    tp: tps[0].clone(),
                    arch: "aarch64".to_owned(),
                },
                Unserved::Busy {
                    slot,
                    holders: vec![("x86-a".to_owned(), "SUSE:Maintenance:2:2".to_owned())],
                    shared: Vec::new(),
                },
                Unserved::NoRefhost { tp: tps[1].clone() },
            ]
        );
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

        let (chosen, slot_candidates, _) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(
            chosen,
            vec!["ppc-a".to_owned(), "x86-a".to_owned()],
            "chosen hosts must follow first-seen slot order"
        );
        assert_eq!(slot_candidates.len(), 2, "two distinct arch slots");
    }

    /// An x86_64 `SLE_HPC 15-SP5` host carrying the `SLES-LTSS 15-SP5` addon
    /// (`hpc`), and `SLES 15-SP5` hosts carrying the same addon (`ltss`).
    fn hpc_ltss_store(hpc: &[&str], ltss: &[&str]) -> Refhosts {
        let sp5 = || Some(Version::new(15, Some(VersionField::Text("SP5".to_owned()))));
        let row = |name: &str, base: &str| Host {
            name: name.to_owned(),
            arch: "x86_64".to_owned(),
            product: Product {
                name: base.to_owned(),
                version: sp5(),
            },
            addons: vec![mtui_types::Addon {
                name: "SLES-LTSS".to_owned(),
                version: sp5(),
            }],
        };
        Refhosts::from_hosts(
            hpc.iter()
                .map(|n| row(n, "SLE_HPC"))
                .chain(ltss.iter().map(|n| row(n, "SLES")))
                .collect(),
        )
    }

    const HPC_TP: &str = "base=SLE_HPC(major=15,minor=SP5);arch=[x86_64]";
    const LTSS_TP: &str = "base=SLES-LTSS(major=15,minor=SP5);arch=[x86_64]";
    const HPC_SLOT: &str = "SLE_HPC|15-SP5|x86_64|";
    const LTSS_SLOT: &str = "SLES-LTSS|15-SP5|x86_64|";
    const SLES_TP: &str = "base=SLES(major=15,minor=SP5);arch=[x86_64]";
    const SLES_SLOT: &str = "SLES|15-SP5|x86_64|";

    /// A host drawn for one slot does not also serve a later slot it matches:
    /// that slot draws its own free candidate.
    #[tokio::test]
    async fn pool_select_host_taken_by_earlier_slot_leaves_later_slot_its_own() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["h1.example.com", "h2.example.com"]);
        assert_eq!(unserved, Vec::new());
    }

    /// Among equally needed candidates, the one an earlier slot took this call
    /// is not drawn again.
    #[tokio::test]
    async fn pool_select_does_not_redraw_host_taken_this_call() {
        let store = hpc_ltss_store(&["h1.example.com", "h2.example.com"], &[]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["h1.example.com", "h2.example.com"]);
        assert_eq!(unserved, Vec::new());
    }

    /// The earlier slot leaves the host a later slot can only get there, even
    /// when that host comes first among its own candidates.
    #[tokio::test]
    async fn pool_select_earlier_slot_prefers_a_host_no_other_slot_needs() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["h2.example.com", "h1.example.com"]);
        assert_eq!(unserved, Vec::new());
    }

    /// A candidate no other slot needs is drawn ahead of a shared one that
    /// comes first, so the shared host stays free when every slot has its own.
    #[tokio::test]
    async fn pool_select_draws_private_candidates_and_leaves_the_shared_one() {
        let sp5 = || Some(Version::new(15, Some(VersionField::Text("SP5".to_owned()))));
        let row = |name: &str, addons: &[&str]| Host {
            name: name.to_owned(),
            arch: "x86_64".to_owned(),
            product: Product {
                name: "SLES".to_owned(),
                version: sp5(),
            },
            addons: addons
                .iter()
                .map(|a| mtui_types::Addon {
                    name: (*a).to_owned(),
                    version: sp5(),
                })
                .collect(),
        };
        let store = Refhosts::from_hosts(vec![
            row("p", &["SLES-LTSS", "sle-ha"]),
            row("q", &["SLES-LTSS"]),
            row("r", &["sle-ha"]),
        ]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![
            LTSS_TP.to_owned(),
            "base=sle-ha(major=15,minor=SP5);arch=[x86_64]".to_owned(),
        ];

        let (chosen, slot_candidates, unserved) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(unserved, Vec::new());
        assert_eq!(slot_candidates.len(), 2);
        assert_eq!(chosen, vec!["q", "r"]);
        assert_eq!(arbiter.owner_of("p"), None);
    }

    /// Slots competing for the same hosts are matched, so each is served when
    /// some assignment serves them all, whatever order they come in.
    #[tokio::test]
    async fn pool_select_serves_every_slot_an_assignment_can_serve() {
        let store = hpc_ltss_store(&["tenar"], &["glinda", "tyr"]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned(), SLES_TP.to_owned()];
        let mut served = std::collections::HashMap::new();

        let (mut chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut served,
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(unserved, Vec::new());
        chosen.sort();
        assert_eq!(chosen, vec!["glinda", "tenar", "tyr"]);
        assert_eq!(
            served,
            std::collections::HashMap::from([
                ("tenar".to_owned(), HPC_SLOT.to_owned()),
                ("glinda".to_owned(), LTSS_SLOT.to_owned()),
                ("tyr".to_owned(), SLES_SLOT.to_owned()),
            ])
        );
    }

    /// A host this update already holds without a slot record takes part in
    /// the matching: it serves the slot only it can serve, and the other slot
    /// draws a free host.
    #[tokio::test]
    async fn pool_select_held_unrecorded_host_is_matched_not_spent_first() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        assert!(arbiter.try_acquire("h1.example.com", &owner));
        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned()];
        let mut served = std::collections::HashMap::new();

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;

        assert_eq!(unserved, Vec::new());
        assert_eq!(chosen, vec!["h2.example.com"]);
        assert_eq!(
            served,
            std::collections::HashMap::from([
                ("h1.example.com".to_owned(), HPC_SLOT.to_owned()),
                ("h2.example.com".to_owned(), LTSS_SLOT.to_owned()),
            ])
        );
    }

    /// A held host serves its slot instead of an extra free host being claimed.
    #[tokio::test]
    async fn pool_select_prefers_a_held_host_over_a_free_one() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64"), ("x86-b", 15, 5, "x86_64")]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        assert!(arbiter.try_acquire("x86-b", &owner));
        let tps = vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()];
        let mut served = std::collections::HashMap::new();

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;

        assert_eq!(chosen, Vec::<String>::new());
        assert_eq!(unserved, Vec::new());
        assert_eq!(arbiter.owner_of("x86-a"), None);
        assert_eq!(served.keys().collect::<Vec<_>>(), vec!["x86-b"]);
    }

    /// Slots sharing held hosts are matched among those first: no free host is
    /// claimed when the held ones can serve every slot.
    #[tokio::test]
    async fn pool_select_held_hosts_serve_every_slot_they_can_before_a_free_one() {
        let store = hpc_ltss_store(&["p1", "p2"], &["s1", "s2"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        for held in ["p1", "s1", "s2"] {
            assert!(arbiter.try_acquire(held, &owner));
        }
        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned(), SLES_TP.to_owned()];
        let mut served = std::collections::HashMap::new();

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;

        assert_eq!(chosen, Vec::<String>::new());
        assert_eq!(unserved, Vec::new());
        assert_eq!(arbiter.owner_of("p2"), None);
        assert_eq!(
            served,
            std::collections::HashMap::from([
                ("p1".to_owned(), HPC_SLOT.to_owned()),
                ("s1".to_owned(), LTSS_SLOT.to_owned()),
                ("s2".to_owned(), SLES_SLOT.to_owned()),
            ])
        );
    }

    /// A claim that another owner wins `p1` and `p2` just before ours.
    fn claim_after_rival_takes_p1_p2(arbiter: &HostArbiter, host: &str, owner: &Owner) -> bool {
        if matches!(host, "p1" | "p2") {
            let rival: Owner = ("rival".to_owned(), "SUSE:Maintenance:2:2".to_owned());
            assert!(arbiter.try_acquire(host, &rival));
        }
        arbiter.try_acquire(host, owner)
    }

    /// Slots whose matched hosts are claimed elsewhere before ours are matched
    /// again around those hosts, jointly, so every slot is still served.
    #[tokio::test]
    async fn pool_select_matches_around_a_host_lost_to_another_claim() {
        let store = hpc_ltss_store(&["p1", "p2", "p3"], &["s1", "s2"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned(), SLES_TP.to_owned()];
        let mut served = std::collections::HashMap::new();

        let (_, _, unserved) = Session::pool_select_claiming(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut served,
            0,
            0,
            no_shuffle,
            claim_after_rival_takes_p1_p2,
        )
        .await;

        assert_eq!(unserved, Vec::new());
        assert_eq!(
            served,
            std::collections::HashMap::from([
                ("s2".to_owned(), LTSS_SLOT.to_owned()),
                ("p3".to_owned(), HPC_SLOT.to_owned()),
                ("s1".to_owned(), SLES_SLOT.to_owned()),
            ])
        );
        assert_ne!(arbiter.owner_of("p1"), Some(owner.clone()));
        assert_ne!(arbiter.owner_of("p2"), Some(owner));
    }

    /// A slot whose only candidates serve other slots is named, not dropped.
    #[tokio::test]
    async fn pool_select_names_slot_whose_candidates_serve_other_slots() {
        let store = hpc_ltss_store(&["h1.example.com"], &[]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["h1.example.com"]);
        assert_eq!(
            unserved,
            vec![Unserved::Shared {
                slot: "SLES-LTSS|15-SP5|x86_64|".to_owned(),
                hosts: vec!["h1.example.com".to_owned()],
            }]
        );
        assert_eq!(
            unserved[0].render(),
            "unserved slot SLES-LTSS 15-SP5 x86_64: every candidate already serves \
             another slot (h1.example.com)"
        );
    }

    /// A rerun keeps each slot on the host it already holds: nothing new is
    /// claimed and no slot is reported.
    #[tokio::test]
    async fn pool_select_rerun_keeps_one_held_host_per_slot() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        assert!(arbiter.try_acquire("h1.example.com", &owner));
        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];
        let mut served = std::collections::HashMap::new();

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;
        assert_eq!(
            chosen,
            vec!["h2.example.com"],
            "h1 serves only the HPC slot"
        );
        assert_eq!(unserved, Vec::new());
        assert_eq!(
            served,
            std::collections::HashMap::from([
                ("h1.example.com".to_owned(), HPC_SLOT.to_owned()),
                ("h2.example.com".to_owned(), LTSS_SLOT.to_owned()),
            ])
        );

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;
        assert_eq!(chosen, Vec::<String>::new());
        assert_eq!(unserved, Vec::new());
    }

    /// A held host stays with the slot it was drawn for when a rerun meets the
    /// slots in the other order.
    #[tokio::test]
    async fn pool_select_rerun_in_other_order_keeps_held_host_with_its_slot() {
        let store = hpc_ltss_store(&["h1.example.com"], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let mut served = std::collections::HashMap::new();
        let shared = vec![Unserved::Shared {
            slot: LTSS_SLOT.to_owned(),
            hosts: vec!["h1.example.com".to_owned()],
        }];

        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];
        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;
        assert_eq!(chosen, vec!["h1.example.com"]);
        assert_eq!(unserved, shared);

        let tps = vec![LTSS_TP.to_owned(), HPC_TP.to_owned()];
        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;
        assert_eq!(chosen, Vec::<String>::new());
        assert_eq!(unserved, shared);
        assert_eq!(
            served,
            std::collections::HashMap::from([("h1.example.com".to_owned(), HPC_SLOT.to_owned())])
        );
    }

    /// A slot whose free candidates are all claimed elsewhere names only those
    /// holders, and the host it lost to an earlier slot as serving that slot.
    #[tokio::test]
    async fn pool_select_busy_slot_does_not_name_a_host_drawn_this_call() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("h2.example.com", &other));
        let tps = vec![HPC_TP.to_owned(), LTSS_TP.to_owned()];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            arbiter,
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["h1.example.com"]);
        assert_eq!(
            unserved.iter().map(Unserved::render).collect::<Vec<_>>(),
            vec![
                "unserved slot SLES-LTSS 15-SP5 x86_64: all 2 candidates claimed \
                 (h2.example.com by SUSE:Maintenance:2:2) or already serving another \
                 slot (h1.example.com)"
            ]
        );
    }

    /// A host held for a slot this call does not ask for is not drawn again
    /// for another slot it matches, even when every free candidate is claimed
    /// elsewhere: that slot is reported busy and the host stays with its own.
    #[tokio::test]
    async fn pool_select_busy_slot_does_not_take_a_host_held_for_an_absent_slot() {
        let store = hpc_ltss_store(&["h1.example.com"], &["h2.example.com"]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("h1.example.com", &owner));
        assert!(arbiter.try_acquire("h2.example.com", &other));
        let mut served =
            std::collections::HashMap::from([("h1.example.com".to_owned(), HPC_SLOT.to_owned())]);
        let tps = vec![LTSS_TP.to_owned()];

        let (chosen, _, unserved) =
            Session::pool_select(&store, &tps, arbiter, &owner, &mut served, 0, 0, no_shuffle)
                .await;

        assert_eq!(chosen, Vec::<String>::new());
        assert_eq!(
            unserved,
            vec![Unserved::Busy {
                slot: LTSS_SLOT.to_owned(),
                holders: vec![(
                    "h2.example.com".to_owned(),
                    "SUSE:Maintenance:2:2".to_owned()
                )],
                shared: vec!["h1.example.com".to_owned()],
            }]
        );
        assert_eq!(
            served,
            std::collections::HashMap::from([("h1.example.com".to_owned(), HPC_SLOT.to_owned())])
        );
    }

    /// Two testplatforms naming the same slot draw one host for it.
    #[tokio::test]
    async fn pool_select_repeated_slot_draws_one_host() {
        let store = multi_host_store(&[("x86-a", 15, 5, "x86_64"), ("x86-b", 15, 5, "x86_64")]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]".to_owned();
        let tps = vec![tp.clone(), tp];

        let (chosen, _, unserved) = Session::pool_select(
            &store,
            &tps,
            test_arbiter(),
            &owner,
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;

        assert_eq!(chosen, vec!["x86-a"]);
        assert_eq!(unserved, Vec::new());
    }

    /// With the arbiter/owner unwired, no testplatform host is resolved at all —
    /// pinning that the connect-every-`search()`-match fallback is gone.
    /// Production always wires both, so this only exercises the fall-through.
    #[tokio::test]
    async fn resolve_and_record_pool_unwired_resolves_no_testplatform_hosts() {
        let (_refhosts_dir, config) = config_with_path_refhosts();
        let mut s = Session::new(config.clone(), false);
        let ref_hosts = vec!["explicit-host".to_owned()];
        // Matches fixture hosts a wired arbiter/owner would otherwise resolve.
        let testplatforms = vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()];

        let (wanted, _) = s
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

    /// A second testplatform add meeting the slots in the other order keeps the
    /// held host with the slot recorded on the report, so the same slot is named.
    #[tokio::test]
    async fn resolve_and_record_pool_rerun_in_other_order_keeps_the_recorded_slot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("refhosts.yml"),
            "default:\n  - name: h1.example.com\n    arch: x86_64\n    product:\n      \
             name: SLE_HPC\n      version:\n        major: 15\n        minor: SP5\n    \
             addons:\n      - name: SLES-LTSS\n        version:\n          major: 15\n          \
             minor: SP5\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.refhosts_resolvers = "path".to_owned();
        config.refhosts_path = dir.path().join("refhosts.yml");
        let rrid = "SUSE:Maintenance:1:1";
        let mut s = Session::new(config.clone(), false);
        seed_active_report(&mut s, rrid, &[], &[]);
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), rrid.to_owned());
        let shared = vec![Unserved::Shared {
            slot: LTSS_SLOT.to_owned(),
            hosts: vec!["h1.example.com".to_owned()],
        }];
        let recorded =
            std::collections::HashMap::from([("h1.example.com".to_owned(), HPC_SLOT.to_owned())]);

        for tps in [[HPC_TP, LTSS_TP], [LTSS_TP, HPC_TP]] {
            let (_, unserved) = s
                .resolve_and_record_pool(
                    &config,
                    Vec::new(),
                    tps.map(str::to_owned).to_vec(),
                    Some(arbiter),
                    Some(owner.clone()),
                    no_shuffle,
                )
                .await;
            assert_eq!(unserved, shared, "{tps:?}");
            assert_eq!(s.metadata().base().pool_slots, recorded, "{tps:?}");
        }
    }

    /// An unavailable inventory is reported rather than looking like "nothing
    /// matched".
    #[tokio::test]
    async fn resolve_and_record_pool_reports_unavailable_inventory() {
        let mut config = Config::default();
        config.refhosts_resolvers = "path".to_owned();
        config.refhosts_path = "/nonexistent/refhosts.yml".into();
        let mut s = Session::new(config.clone(), false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());

        let (wanted, unserved) = s
            .resolve_and_record_pool(
                &config,
                Vec::new(),
                vec!["base=sles(major=15,minor=5);arch=[x86_64]".to_owned()],
                Some(test_arbiter()),
                Some(owner),
                no_shuffle,
            )
            .await;

        assert!(wanted.is_empty());
        assert_eq!(unserved, vec![Unserved::InventoryUnavailable]);
    }

    /// Every reason renders as one line, the slot key as `product version arch
    /// +addon`.
    #[test]
    fn unserved_reasons_render_one_line_each() {
        let slot = "sles|15.5|aarch64|ha,we".to_owned();
        let reasons = [
            Unserved::NoRefhost {
                tp: "base=sles(major=15,minor=5);arch=[s390x]".to_owned(),
            },
            Unserved::NoArchRefhost {
                tp: "base=sles(major=15,minor=5);arch=[x86_64,s390x]".to_owned(),
                arch: "s390x".to_owned(),
            },
            Unserved::InventoryUnavailable,
            Unserved::Busy {
                slot: slot.clone(),
                holders: vec![
                    ("h1".to_owned(), "SUSE:Maintenance:2:2".to_owned()),
                    ("h2".to_owned(), "SUSE:Maintenance:3:3".to_owned()),
                ],
                shared: Vec::new(),
            },
            Unserved::Busy {
                slot: slot.clone(),
                holders: vec![("h11".to_owned(), "SUSE:Maintenance:2:2".to_owned())],
                shared: vec!["h12".to_owned()],
            },
            Unserved::Unconnectable {
                slot: slot.clone(),
                failures: vec![
                    ("h3".to_owned(), HostFailure::Unreachable),
                    (
                        "h4".to_owned(),
                        HostFailure::ClaimedBy(Some("SUSE:Maintenance:9:9".to_owned())),
                    ),
                    ("h5".to_owned(), HostFailure::ClaimedBy(None)),
                    ("h8".to_owned(), HostFailure::ClaimFailed),
                    ("h9".to_owned(), sles15_ro_refusal()),
                    (
                        "h10".to_owned(),
                        HostFailure::Unusable {
                            gap: CommandSetGap::UnreadableBaseProduct,
                            drift: None,
                        },
                    ),
                ],
            },
            Unserved::BudgetSpent {
                slot: "sles|15.5|x86_64|".to_owned(),
                tried: vec!["h6".to_owned(), "h7".to_owned()],
            },
        ];
        let out: Vec<String> = reasons.iter().map(Unserved::render).collect();
        assert!(out.iter().all(|l| !l.contains('\n')));
        insta::assert_snapshot!(out.join("\n"));
    }

    /// Once the shared budget is spent, the remaining slots are abandoned rather
    /// than every sibling of every slot walked. Each candidate is a black-hole
    /// listener (bound, never `accept()`ed) costing a full `connect_timeout = 1s`
    /// to fail. The first of each of the 3 slots is the failed primary, so the 6
    /// siblings would take ~6s while the `4 * connect_timeout` budget must cut
    /// the walk off after about 4.
    #[tokio::test]
    async fn connect_pool_backups_stops_once_budget_is_spent() {
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let mut s = Session::new(config.clone(), false);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, &[], &[]);

        let listeners: Vec<std::net::TcpListener> = (0..9)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind"))
            .collect();
        let candidates: Vec<String> = listeners.iter().map(listener_addr).collect();
        let primaries: Vec<String> = candidates.chunks(3).map(|c| c[0].clone()).collect();

        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), rrid.to_owned());
        let names: Vec<String> = (0..3).map(|i| format!("slot-{i}")).collect();
        let slots: Vec<(&str, &[String])> = candidates
            .chunks(3)
            .zip(&names)
            .map(|(c, n)| (n.as_str(), c))
            .collect();
        let claims: Vec<&str> = primaries.iter().map(String::as_str).collect();
        wire_pool(&mut s, arbiter, &owner, &slots, &claims);

        let failures = primaries
            .iter()
            .map(|p| (p.clone(), HostFailure::Unreachable))
            .collect();
        let (drift, unserved, _) = tokio::time::timeout(
            Duration::from_secs(30),
            s.connect_pool_backups(
                &config,
                rrid,
                &primaries,
                failures,
                None,
                &primaries.iter().cloned().collect(),
                &std::collections::HashMap::new(),
                true,
                HostArbiter::try_acquire,
            ),
        )
        .await
        .expect("must not hang past the outer test wrapper");

        assert!(drift.is_empty(), "no black hole ever connects: {drift:?}");
        let mut named: Vec<&str> = unserved
            .iter()
            .map(|u| match u {
                Unserved::Unconnectable { slot, .. } | Unserved::BudgetSpent { slot, .. } => {
                    slot.as_str()
                }
                other => panic!("unexpected reason {other:?}"),
            })
            .collect();
        named.sort_unstable();
        assert_eq!(
            named,
            ["slot-0", "slot-1", "slot-2"],
            "every abandoned slot is named exactly once"
        );
        assert!(
            unserved
                .iter()
                .any(|u| matches!(u, Unserved::BudgetSpent { .. })),
            "the slots the budget cut off must say so: {unserved:?}"
        );
        let tried: usize = unserved
            .iter()
            .map(|u| match u {
                Unserved::Unconnectable { failures, .. } => failures.len(),
                Unserved::BudgetSpent { tried, .. } => tried.len(),
                _ => 0,
            })
            .sum();
        assert!(
            tried < 9,
            "shared budget must stop the walk short of all 6 siblings, tried {} of them",
            tried - 3
        );
        drop(listeners);
    }

    /// `n` distinct unused local addresses: bound together to learn free ports,
    /// then released, so a connect is refused at once. The next `bind(0)` may
    /// be handed a released port, so take these after the test's listeners.
    fn closed_addrs(n: usize) -> Vec<String> {
        let held: Vec<std::net::TcpListener> = (0..n)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind"))
            .collect();
        held.iter().map(listener_addr).collect()
    }

    fn closed_addr() -> String {
        closed_addrs(1).remove(0)
    }

    /// A slot naming one address twice would let the backup walk skip a
    /// sibling as the primary it already tried.
    #[test]
    #[should_panic(expected = "repeated candidate")]
    fn wire_pool_refuses_a_repeated_candidate() {
        let (_refhosts_dir, mut s, arbiter, owner, _) = pool_session(1, 0);
        let addr = closed_addr();
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[addr.clone(), addr])],
            &[],
        );
    }

    fn listener_addr(l: &std::net::TcpListener) -> String {
        format!("127.0.0.1:{}", l.local_addr().unwrap().port())
    }

    /// Whether anything dialled `l` since it was bound (a backlogged connection
    /// is still `accept`able after the client gave up).
    fn was_dialled(l: &std::net::TcpListener) -> bool {
        l.set_nonblocking(true).expect("nonblocking");
        l.accept().is_ok()
    }

    /// Wires the pool seam onto the active report: the arbiter and `owner`, the
    /// recorded `slots`, and `claims` as both pool claims and arbiter-held.
    fn wire_pool(
        s: &mut Session,
        arbiter: &'static HostArbiter,
        owner: &Owner,
        slots: &[(&str, &[String])],
        claims: &[&str],
    ) {
        for c in claims {
            assert!(arbiter.try_acquire(c, owner), "{c} must start free");
        }
        let base = s.metadata_mut().base_mut();
        base.arbiter = Some(arbiter);
        base.owner = Some(owner.clone());
        for (slot, cands) in slots {
            let distinct: std::collections::HashSet<&String> = cands.iter().collect();
            assert_eq!(
                distinct.len(),
                cands.len(),
                "slot {slot} has a repeated candidate: {cands:?}"
            );
            base.slot_candidates
                .insert((*slot).to_owned(), cands.to_vec());
        }
        for c in claims {
            base.pool_claims.insert((*c).to_owned());
        }
    }

    /// A slot that already has a host attached is served: re-running `add_host`
    /// must neither drop its pool claim nor dial a sibling, and the attached
    /// host keeps its command log.
    #[tokio::test]
    async fn backup_keeps_attached_slot_host() {
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let mut s = Session::new(config, false);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, &[], &[]);
        let sibling = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), rrid.to_owned());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &["mock-a".to_owned(), listener_addr(&sibling)])],
            &["mock-a"],
        );
        let conn = MockConnection::new("mock-a").with_default(CommandLog::new("", "ok", "", 0, 0));
        s.targets_mut().add(Target::with_connection(
            "mock-a",
            TargetState::Enabled,
            Box::new(conn),
        ));
        s.targets_mut().run("echo marker").await;
        assert_eq!(s.targets().get("mock-a").unwrap().out().len(), 1);

        s.connect_and_add_hosts(vec![], rrid, &std::collections::HashSet::new())
            .await;

        assert!(
            s.metadata().base().pool_claims.contains("mock-a"),
            "a served slot keeps its pool claim"
        );
        assert_eq!(arbiter.owner_of("mock-a"), Some(owner));
        assert_eq!(
            s.targets().get("mock-a").unwrap().out().len(),
            1,
            "the attached host was replaced or reset"
        );
        assert!(
            !was_dialled(&sibling),
            "a served slot must not be backed up"
        );
    }

    /// `add_host -t` names hosts that are not pool claims, so it must not back
    /// up slots nobody asked about.
    #[tokio::test]
    async fn named_add_does_not_back_up_unrelated_slots() {
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let mut s = Session::new(config, false);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, &[], &[]);
        let unrelated = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), rrid.to_owned());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[listener_addr(&unrelated)])],
            &[],
        );

        s.add_named_hosts(vec![closed_addr()]).await;

        assert!(s.targets().is_empty());
        assert!(
            !was_dialled(&unrelated),
            "an unrelated slot was claimed and dialled"
        );
    }

    /// The case backup exists for: a pool-claimed primary that fails to connect
    /// is replaced by a sibling from its slot, and every claim is released once
    /// the sibling fails too.
    #[tokio::test]
    async fn backup_retries_failed_primary() {
        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let mut s = Session::new(config, false);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, &[], &[]);
        let sibling = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let sibling_addr = listener_addr(&sibling);
        let primary = closed_addr();
        let arbiter = test_arbiter();
        let owner: Owner = ("reg".to_owned(), rrid.to_owned());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[primary.clone(), sibling_addr.clone()])],
            &[&primary],
        );

        let unserved = s
            .connect_and_add_hosts(
                vec![primary.clone()],
                rrid,
                &std::collections::HashSet::from([primary.clone()]),
            )
            .await
            .0;

        assert!(was_dialled(&sibling), "the sibling was never tried");
        assert_eq!(
            unserved,
            vec![Unserved::Unconnectable {
                slot: "slot".to_owned(),
                failures: vec![
                    (primary.clone(), HostFailure::Unreachable),
                    (sibling_addr.clone(), HostFailure::Unreachable),
                ],
            }]
        );
        assert!(s.targets().is_empty());
        assert_eq!(arbiter.owner_of(&primary), None);
        assert_eq!(arbiter.owner_of(&sibling_addr), None);
        assert!(s.metadata().base().pool_claims.is_empty());
    }

    /// The rendered refusal of a SLES 15 SP6 host with a read-only root,
    /// whose key no update-workflow table serves.
    const SLES15_RO_REFUSAL: &str = "no command set for SLES 15-SP6, transactional";

    /// The refusal of a SLES 15 SP6 host with a read-only root, outside
    /// refhosts.yml (so with no drift).
    fn sles15_ro_refusal() -> HostFailure {
        HostFailure::Unusable {
            gap: CommandSetGap::NoCommandSet(mtui_testreport::reports::HostProductKey::new(
                "SLES",
                "15-SP6",
                mtui_hosts::Transactional::Yes,
            )),
            drift: None,
        }
    }

    /// Whether `conn` carries either remote lock mtui writes on attach.
    fn holds_a_lock(conn: &MockConnection) -> bool {
        conn.file_contents(mtui_hosts::POOL_LOCK_PATH).is_some()
            || conn.file_contents(mtui_hosts::TARGET_LOCK_PATH).is_some()
    }

    /// A display-capturing session over an active report naming `hostnames`
    /// and `testplatforms`, with its own arbiter (the process-global one would
    /// arbitrate these hosts against other tests) and a PI `lock_comment`, so
    /// an attach writes the operation lock, and the pool claim for a pool draw.
    fn locking_session(
        config: Config,
        hostnames: &[&str],
        testplatforms: &[&str],
    ) -> (Session, crate::commands::testkit::Buffer) {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let buf = crate::commands::testkit::Buffer::new();
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
        let mut s = Session::with_display(config, false, display);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, hostnames, testplatforms);
        let base = s.metadata_mut().base_mut();
        base.arbiter = Some(test_arbiter());
        base.owner = Some(("reg".to_owned(), rrid.to_owned()));
        base.lock_comment = "mtui pi".to_owned();
        (s, buf)
    }

    /// A pool-drawn host with no command set is a failed draw: its slot goes
    /// to a sibling that has one, and the refused host is neither claimed nor
    /// locked.
    #[tokio::test]
    async fn pool_host_without_command_set_yields_its_slot_to_a_sibling() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        s.metadata_mut().base_mut().lock_comment = "mtui pi".to_owned();
        let (ro, rw) = ("ro-primary.invalid", "rw-sibling.invalid");
        let ro_conn = product_host(ro, "SLES", "15", "6", true);
        let rw_conn = product_host(rw, "SLES", "15", "6", false);
        s.inject_mock_connection(ro, ro_conn.clone());
        s.inject_mock_connection(rw, rw_conn.clone());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[ro.to_owned(), rw.to_owned()])],
            &[ro],
        );

        let (unserved, failures) = s
            .connect_and_add_hosts(
                vec![ro.to_owned()],
                rrid,
                &std::collections::HashSet::from([ro.to_owned()]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![rw.to_owned()]);
        assert_eq!(failures, vec![(ro.to_owned(), sles15_ro_refusal())]);
        assert_eq!(arbiter.owner_of(ro), None);
        assert!(!s.metadata().base().pool_claims.contains(ro));
        assert!(!holds_a_lock(&ro_conn), "the refused host was locked");
        assert!(
            rw_conn.file_contents(mtui_hosts::POOL_LOCK_PATH).is_some()
                && rw_conn
                    .file_contents(mtui_hosts::TARGET_LOCK_PATH)
                    .is_some(),
            "the sibling's locks must be observable, or the check above proves nothing"
        );
    }

    /// A backup sibling refused before a later sibling serves the slot is
    /// named like the refused primary, though the slot reports nothing.
    #[tokio::test]
    async fn refused_backup_on_a_served_slot_is_named() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (ro, ro_backup, rw) = (
            "ro-served-primary.invalid",
            "ro-served-backup.invalid",
            "rw-served-backup.invalid",
        );
        let ro_backup_conn = product_host(ro_backup, "SLES", "15", "6", true);
        s.inject_mock_connection(ro, product_host(ro, "SLES", "15", "6", true));
        s.inject_mock_connection(ro_backup, ro_backup_conn.clone());
        s.inject_mock_connection(rw, product_host(rw, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[(
                "slot",
                &[ro.to_owned(), ro_backup.to_owned(), rw.to_owned()],
            )],
            &[ro],
        );

        let (unserved, failures) = s
            .connect_and_add_hosts(
                vec![ro.to_owned()],
                rrid,
                &std::collections::HashSet::from([ro.to_owned()]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![rw.to_owned()]);
        let named: Vec<&str> = refused_hosts(&failures, &unserved)
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        assert_eq!(named, vec![ro_backup, ro]);
        assert!(!holds_a_lock(&ro_backup_conn));
    }

    /// A slot whose own draw failed is backed up even though another slot's
    /// attached host is one of its candidates.
    #[tokio::test]
    async fn backup_retries_slot_whose_candidate_serves_another_slot() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (shared, backup) = ("h1.example.com", "h3.example.com");
        let dead = closed_addr();
        s.inject_mock_connection(shared, product_host(shared, "SLES", "15", "6", false));
        s.inject_mock_connection(backup, product_host(backup, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[
                ("hpc", &[shared.to_owned()]),
                (
                    "ltss",
                    &[shared.to_owned(), dead.clone(), backup.to_owned()],
                ),
            ],
            &[shared, &dead],
        );
        let base = s.metadata_mut().base_mut();
        base.pool_slots.insert(shared.to_owned(), "hpc".to_owned());
        base.pool_slots.insert(dead.clone(), "ltss".to_owned());

        let (unserved, _) = s
            .connect_and_add_hosts(
                vec![shared.to_owned(), dead.clone()],
                rrid,
                &std::collections::HashSet::from([shared.to_owned(), dead.clone()]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        let mut names = s.targets().names();
        names.sort();
        assert_eq!(names, vec![shared.to_owned(), backup.to_owned()]);
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([
                (shared.to_owned(), "hpc".to_owned()),
                (backup.to_owned(), "ltss".to_owned()),
            ])
        );
    }

    /// Two slots whose primaries failed share one backup: the slot that loses
    /// it is named, not dropped as if the other slot's host served it.
    #[tokio::test]
    async fn backup_taken_by_one_slot_leaves_the_other_unserved() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let backup = "h3.example.com";
        let (dead_a, dead_b) = (closed_addr(), closed_addr());
        s.inject_mock_connection(backup, product_host(backup, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[
                ("a", &[dead_a.clone(), backup.to_owned()]),
                ("b", &[dead_b.clone(), backup.to_owned()]),
            ],
            &[&dead_a, &dead_b],
        );
        let base = s.metadata_mut().base_mut();
        base.pool_slots.insert(dead_a.clone(), "a".to_owned());
        base.pool_slots.insert(dead_b.clone(), "b".to_owned());

        let (unserved, _) = s
            .connect_and_add_hosts(
                vec![dead_a.clone(), dead_b.clone()],
                rrid,
                &std::collections::HashSet::from([dead_a, dead_b]),
            )
            .await;

        assert_eq!(s.targets().names(), vec![backup.to_owned()]);
        assert!(
            matches!(&unserved[..], [Unserved::Unconnectable { slot, .. }] if slot == "b"),
            "{unserved:?}"
        );
    }

    /// Slots backed up together are matched to their siblings jointly: the
    /// earlier slot leaves the shared backup to the slot that has no other.
    #[tokio::test]
    async fn backup_slots_are_matched_to_siblings_jointly() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (shared, spare) = ("h3.example.com", "h4.example.com");
        let (dead_a, dead_b) = (closed_addr(), closed_addr());
        s.inject_mock_connection(shared, product_host(shared, "SLES", "15", "6", false));
        s.inject_mock_connection(spare, product_host(spare, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[
                ("a", &[dead_a.clone(), shared.to_owned(), spare.to_owned()]),
                ("b", &[dead_b.clone(), shared.to_owned()]),
            ],
            &[&dead_a, &dead_b],
        );
        let base = s.metadata_mut().base_mut();
        base.pool_slots.insert(dead_a.clone(), "a".to_owned());
        base.pool_slots.insert(dead_b.clone(), "b".to_owned());

        let (unserved, _) = s
            .connect_and_add_hosts(
                vec![dead_a.clone(), dead_b.clone()],
                rrid,
                &std::collections::HashSet::from([dead_a, dead_b]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([
                (spare.to_owned(), "a".to_owned()),
                (shared.to_owned(), "b".to_owned()),
            ])
        );
    }

    /// A claim that never wins `h3.example.com`, though nobody holds it.
    fn claim_refusing_h3(arbiter: &HostArbiter, host: &str, owner: &Owner) -> bool {
        host != "h3.example.com" && arbiter.try_acquire(host, owner)
    }

    /// A backup sibling whose claim is lost after the match is matched around:
    /// the slot is served by its next sibling within the budget.
    #[tokio::test]
    async fn backup_matches_around_a_sibling_lost_to_another_claim() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (lost, spare) = ("h3.example.com", "h4.example.com");
        let dead = closed_addr();
        s.inject_mock_connection(lost, product_host(lost, "SLES", "15", "6", false));
        s.inject_mock_connection(spare, product_host(spare, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("a", &[dead.clone(), lost.to_owned(), spare.to_owned()])],
            &[&dead],
        );
        s.metadata_mut()
            .base_mut()
            .pool_slots
            .insert(dead.clone(), "a".to_owned());
        let config = s.config.clone();
        let injected = s.injected_connections();

        let (_, unserved, _) = s
            .connect_pool_backups(
                &config,
                rrid,
                std::slice::from_ref(&dead),
                std::collections::HashMap::from([(dead.clone(), HostFailure::Unreachable)]),
                None,
                &std::collections::HashSet::from([dead.clone()]),
                &injected,
                true,
                claim_refusing_h3,
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![spare.to_owned()]);
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([(spare.to_owned(), "a".to_owned())])
        );
        assert_eq!(arbiter.owner_of(lost), None);
    }

    /// A backed-up slot tries its own sibling, not a host attached for another
    /// slot, and that host stays recorded for the slot it serves.
    #[tokio::test]
    async fn backup_does_not_draw_a_host_attached_for_another_slot() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (attached, backup) = ("h1.example.com", "h3.example.com");
        let dead = closed_addr();
        s.inject_mock_connection(attached, product_host(attached, "SLES", "15", "6", false));
        s.inject_mock_connection(backup, product_host(backup, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[
                ("a", &[attached.to_owned()]),
                ("b", &[dead.clone(), attached.to_owned(), backup.to_owned()]),
            ],
            &[attached, &dead],
        );
        let base = s.metadata_mut().base_mut();
        base.pool_slots.insert(attached.to_owned(), "a".to_owned());
        base.pool_slots.insert(dead.clone(), "b".to_owned());
        let conn = MockConnection::new(attached).with_default(CommandLog::new("", "ok", "", 0, 0));
        s.targets_mut().add(Target::with_connection(
            attached,
            TargetState::Enabled,
            Box::new(conn),
        ));

        let (unserved, _) = s
            .connect_and_add_hosts(
                vec![dead.clone()],
                rrid,
                &std::collections::HashSet::from([dead]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        let mut names = s.targets().names();
        names.sort();
        assert_eq!(names, vec![attached.to_owned(), backup.to_owned()]);
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([
                (attached.to_owned(), "a".to_owned()),
                (backup.to_owned(), "b".to_owned()),
            ])
        );
        assert!(s.metadata().base().pool_claims.contains(attached));
        assert_eq!(arbiter.owner_of(attached), Some(owner));
    }

    /// A backed-up slot with no free sibling left does not wait for, and take,
    /// a host attached for another slot: it is named unserved.
    #[tokio::test]
    async fn backup_without_a_free_sibling_does_not_take_a_host_attached_for_another_slot() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let attached = "h1.example.com";
        let dead = closed_addr();
        s.inject_mock_connection(attached, product_host(attached, "SLES", "15", "6", false));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[
                ("a", &[attached.to_owned()]),
                ("b", &[dead.clone(), attached.to_owned()]),
            ],
            &[attached, &dead],
        );
        let base = s.metadata_mut().base_mut();
        base.pool_slots.insert(attached.to_owned(), "a".to_owned());
        base.pool_slots.insert(dead.clone(), "b".to_owned());
        let conn = MockConnection::new(attached).with_default(CommandLog::new("", "ok", "", 0, 0));
        s.targets_mut().add(Target::with_connection(
            attached,
            TargetState::Enabled,
            Box::new(conn),
        ));

        let (unserved, _) = s
            .connect_and_add_hosts(
                vec![dead.clone()],
                rrid,
                &std::collections::HashSet::from([dead]),
            )
            .await;

        assert!(
            matches!(&unserved[..], [Unserved::Unconnectable { slot, .. }] if slot == "b"),
            "{unserved:?}"
        );
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([(attached.to_owned(), "a".to_owned())])
        );
    }

    /// With no sibling that has a command set, the slot is unserved and the
    /// reason names each host's key.
    #[tokio::test]
    async fn pool_slot_without_a_command_set_sibling_names_the_key() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (first, second) = ("ro-first.invalid", "ro-second.invalid");
        let second_conn = product_host(second, "SLES", "15", "6", true);
        s.inject_mock_connection(first, product_host(first, "SLES", "15", "6", true));
        s.inject_mock_connection(second, second_conn.clone());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[first.to_owned(), second.to_owned()])],
            &[first],
        );

        let unserved = s
            .connect_and_add_hosts(
                vec![first.to_owned()],
                rrid,
                &std::collections::HashSet::from([first.to_owned()]),
            )
            .await
            .0;

        assert_eq!(
            unserved,
            vec![Unserved::Unconnectable {
                slot: "slot".to_owned(),
                failures: vec![
                    (first.to_owned(), sles15_ro_refusal()),
                    (second.to_owned(), sles15_ro_refusal()),
                ],
            }]
        );
        assert!(
            unserved[0]
                .render()
                .contains(&format!("{second}: {SLES15_RO_REFUSAL}")),
            "{}",
            unserved[0].render()
        );
        assert!(s.targets().is_empty());
        assert!(s.metadata().base().pool_claims.is_empty());
        assert_eq!(arbiter.owner_of(second), None);
        assert!(!holds_a_lock(&second_conn));
    }

    /// A template's named host with no command set is refused at load time,
    /// before any claim or lock, and the load's output says so: under
    /// `mtui-mcp` a log line is never seen.
    #[tokio::test]
    async fn template_named_host_without_command_set_is_refused_on_the_display() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let host = "ro-named.invalid";
        let (mut s, buf) = locking_session(config, &[host], &[]);
        let conn = product_host(host, "SLES", "15", "6", true);
        s.inject_mock_connection(host, conn.clone());

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        assert!(s.targets().is_empty());
        assert!(!holds_a_lock(&conn), "the refused host was locked");
        assert_eq!(
            buf.contents(),
            format!("skipped {host}: {SLES15_RO_REFUSAL}\n")
        );
    }

    /// The same session attaches and locks a host with a command set, so
    /// [`template_named_host_without_command_set_is_refused_on_the_display`]'s
    /// lock check can fail.
    #[tokio::test]
    async fn template_named_host_with_a_command_set_is_locked() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let host = "rw-named.invalid";
        let (mut s, _buf) = locking_session(config, &[host], &[]);
        let conn = product_host(host, "SLES", "15", "6", false);
        s.inject_mock_connection(host, conn.clone());

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        assert_eq!(s.targets().names(), vec![host.to_owned()]);
        // A template's named host takes no pool claim; the PI autolock is
        // what an attach writes.
        assert!(conn.file_contents(mtui_hosts::TARGET_LOCK_PATH).is_some());
    }

    /// Under `load_template`'s pool draw every refused host is named, the
    /// backup siblings tried after the primary included, and none is locked.
    #[tokio::test]
    async fn load_template_names_every_refused_pool_host() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let candidates = resolve_testplatform_hosts(&config, &[tp.to_owned()]).await;
        assert!(
            candidates.len() >= 2,
            "a slot needs a sibling: {candidates:?}"
        );
        let (mut s, buf) = locking_session(config, &[], &[tp]);
        let conns: Vec<MockConnection> = candidates
            .iter()
            .map(|h| product_host(h, "SLES", "15", "5", true))
            .collect();
        for (h, c) in candidates.iter().zip(&conns) {
            s.inject_mock_connection(h, c.clone());
        }

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        assert!(s.targets().is_empty());
        let out = buf.contents();
        for (h, c) in candidates.iter().zip(&conns) {
            assert!(
                out.contains(&format!(
                    "skipped {h}: no command set for SLES 15-SP5, transactional"
                )),
                "{h} not named: {out:?}"
            );
            assert!(!holds_a_lock(c), "{h} was locked");
        }
    }

    /// An operation-lock line written by `user` as `pid`, `age` seconds ago.
    fn op_lock_line(user: &str, pid: u32, age: u64) -> Vec<u8> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("{}:{user}:{pid}", now - age).into_bytes()
    }

    /// A host with a command set whose operation lock file holds `lock`.
    fn locked_host(host: &str, lock: Vec<u8>) -> MockConnection {
        crate::commands::testkit::product_host(host, "SLES", "15", "6", false)
            .with_file(mtui_hosts::TARGET_LOCK_PATH, lock)
    }

    /// A host with a command set and no lock.
    fn free_host(host: &str) -> MockConnection {
        crate::commands::testkit::product_host(host, "SLES", "15", "6", false)
    }

    /// Draws `primary` from a slot of `candidates` in a fresh pool session
    /// and returns the session, the arbiter and what the attach reported.
    async fn draw_pool_primary(
        conns: &[(&str, MockConnection)],
        primary: &str,
    ) -> (
        tempfile::TempDir,
        Session,
        &'static HostArbiter,
        Vec<Unserved>,
        Vec<(String, HostFailure)>,
    ) {
        let (refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        for (h, c) in conns {
            s.inject_mock_connection(h, c.clone());
        }
        let candidates: Vec<String> = conns.iter().map(|(h, _)| (*h).to_owned()).collect();
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &candidates)],
            &[primary],
        );
        let (unserved, failures) = s
            .connect_and_add_hosts(
                vec![primary.to_owned()],
                rrid,
                &std::collections::HashSet::from([primary.to_owned()]),
            )
            .await;
        (refhosts_dir, s, arbiter, unserved, failures)
    }

    /// A pool draw whose primary another owner's live operation lock holds
    /// goes to a sibling; the primary is neither claimed nor relocked.
    #[tokio::test]
    async fn pool_host_under_a_foreign_operation_lock_yields_its_slot_to_a_sibling() {
        let (held, free) = ("held-primary.invalid", "free-sibling.invalid");
        let lock = op_lock_line("otheruser", 99, 60);
        let held_conn = locked_host(held, lock.clone());
        let free_conn = free_host(free);

        let (_dir, s, arbiter, unserved, failures) = draw_pool_primary(
            &[(held, held_conn.clone()), (free, free_conn.clone())],
            held,
        )
        .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![free.to_owned()]);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, held);
        assert!(
            failures[0]
                .1
                .render()
                .starts_with("operation lock held by otheruser since "),
            "{failures:?}"
        );
        assert_eq!(arbiter.owner_of(held), None);
        assert!(!s.metadata().base().pool_claims.contains(held));
        assert!(
            held_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_none()
        );
        assert_eq!(
            held_conn.file_contents(mtui_hosts::TARGET_LOCK_PATH),
            Some(lock)
        );
        assert!(
            free_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_some(),
            "the sibling's claim must be observable, or the check above proves nothing"
        );
    }

    /// The same user's lock from another mtui process is foreign: this
    /// session could not take that host's operation lock either.
    #[tokio::test]
    async fn pool_host_locked_by_another_process_of_ours_is_skipped() {
        let user = Config::default().session_user;
        let (held, free) = ("own-user-primary.invalid", "own-user-sibling.invalid");
        let held_conn = locked_host(held, op_lock_line(&user, std::process::id() + 1, 60));

        let (_dir, s, _arbiter, _unserved, failures) =
            draw_pool_primary(&[(held, held_conn.clone()), (free, free_host(free))], held).await;

        assert_eq!(s.targets().names(), vec![free.to_owned()]);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            held_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_none()
        );
    }

    /// A lock this process holds is no reason to skip the host.
    #[tokio::test]
    async fn pool_host_under_our_own_operation_lock_attaches() {
        let user = Config::default().session_user;
        let (mine, other) = ("own-lock-primary.invalid", "own-lock-sibling.invalid");
        let conn = locked_host(mine, op_lock_line(&user, std::process::id(), 60));

        let (_dir, s, _arbiter, unserved, failures) =
            draw_pool_primary(&[(mine, conn.clone()), (other, free_host(other))], mine).await;

        assert!(unserved.is_empty() && failures.is_empty(), "{failures:?}");
        assert_eq!(s.targets().names(), vec![mine.to_owned()]);
        assert!(conn.file_contents(mtui_hosts::POOL_LOCK_PATH).is_some());
    }

    /// A stale foreign lock is reaped on connect, so its host is drawn.
    #[tokio::test]
    async fn pool_host_under_a_stale_foreign_lock_is_reaped_and_attached() {
        let (stale, other) = ("stale-primary.invalid", "stale-sibling.invalid");
        let conn = locked_host(stale, op_lock_line("otheruser", 99, 2 * 86_400));

        let (_dir, s, _arbiter, unserved, failures) =
            draw_pool_primary(&[(stale, conn.clone()), (other, free_host(other))], stale).await;

        assert!(unserved.is_empty() && failures.is_empty(), "{failures:?}");
        assert_eq!(s.targets().names(), vec![stale.to_owned()]);
        assert!(conn.file_contents(mtui_hosts::TARGET_LOCK_PATH).is_none());
        assert!(conn.file_contents(mtui_hosts::POOL_LOCK_PATH).is_some());
    }

    /// An operation lock that cannot be read does not skip a pool draw, as it
    /// does not fail the connect.
    #[tokio::test]
    async fn pool_host_with_an_unreadable_operation_lock_attaches() {
        let (host, other) = ("unreadable-primary.invalid", "unreadable-sibling.invalid");
        let conn = free_host(host).with_open_error(mtui_hosts::TARGET_LOCK_PATH);

        let (_dir, s, _arbiter, unserved, failures) =
            draw_pool_primary(&[(host, conn), (other, free_host(other))], host).await;

        assert!(unserved.is_empty() && failures.is_empty(), "{failures:?}");
        assert_eq!(s.targets().names(), vec![host.to_owned()]);
    }

    /// With every candidate under a foreign lock the slot is unserved, its
    /// line names the holder per host, and no claim is left anywhere.
    #[tokio::test]
    async fn pool_slot_with_every_candidate_locked_names_the_holder() {
        let (first, second) = ("locked-first.invalid", "locked-second.invalid");
        let first_conn = locked_host(first, op_lock_line("otheruser", 99, 60));
        let second_conn = locked_host(second, op_lock_line("thirduser", 98, 60));

        let (_dir, s, arbiter, unserved, _failures) = draw_pool_primary(
            &[(first, first_conn.clone()), (second, second_conn.clone())],
            first,
        )
        .await;

        assert_eq!(unserved.len(), 1, "{unserved:?}");
        let line = unserved[0].render();
        assert!(
            line.starts_with("unserved slot slot: no candidate connected (")
                && line.contains(&format!("{first}: operation lock held by otheruser since "))
                && line.contains(&format!(
                    "{second}: operation lock held by thirduser since "
                )),
            "{line}"
        );
        assert!(s.targets().is_empty());
        assert!(s.metadata().base().pool_claims.is_empty());
        assert_eq!(arbiter.owner_of(first), None);
        assert_eq!(arbiter.owner_of(second), None);
        assert!(
            first_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_none()
        );
        assert!(
            second_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_none()
        );
    }

    /// The backup step skips a sibling under a foreign lock for the next
    /// one, and names it though the slot is served.
    #[tokio::test]
    async fn backup_skips_a_locked_sibling() {
        use crate::commands::testkit::product_host;

        let (ro, held, free) = (
            "ro-backup-primary.invalid",
            "held-backup.invalid",
            "free-backup.invalid",
        );
        let held_conn = locked_host(held, op_lock_line("otheruser", 99, 60));

        let (_dir, s, arbiter, unserved, failures) = draw_pool_primary(
            &[
                (ro, product_host(ro, "SLES", "15", "6", true)),
                (held, held_conn.clone()),
                (free, free_host(free)),
            ],
            ro,
        )
        .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![free.to_owned()]);
        assert_eq!(arbiter.owner_of(held), None);
        assert!(
            held_conn
                .file_contents(mtui_hosts::POOL_LOCK_PATH)
                .is_none()
        );
        let named: Vec<String> = refused_hosts(&failures, &unserved)
            .into_iter()
            .map(|(h, f)| format!("{h}: {}", f.render()))
            .collect();
        assert_eq!(named.len(), 2, "{named:?}");
        assert!(
            named[0].starts_with(&format!("{held}: operation lock held by otheruser since ")),
            "{named:?}"
        );
        assert_eq!(named[1], format!("{ro}: {SLES15_RO_REFUSAL}"));
    }

    /// A host named explicitly attaches under a foreign operation lock: only
    /// a pool draw is skipped for it.
    #[tokio::test]
    async fn named_host_under_a_foreign_operation_lock_still_attaches() {
        let (_refhosts_dir, mut s, arbiter, owner, _rrid) = pool_session(1, 0);
        wire_pool(&mut s, arbiter, &owner, &[], &[]);
        let host = "named-locked.invalid";
        let lock = op_lock_line("otheruser", 99, 60);
        let conn = locked_host(host, lock.clone());
        s.inject_mock_connection(host, conn.clone());

        let refused = s.add_named_hosts(vec![host.to_owned()]).await;

        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(s.targets().names(), vec![host.to_owned()]);
        assert_eq!(conn.file_contents(mtui_hosts::TARGET_LOCK_PATH), Some(lock));
        assert!(
            conn.file_contents(mtui_hosts::POOL_LOCK_PATH).is_some(),
            "a named host is claimed like a pool draw, so only the draw rule can tell them apart"
        );
    }

    /// A template's named host attaches under a foreign operation lock too:
    /// it is no pool draw, though `load_template` connects it with the draws.
    #[tokio::test]
    async fn template_named_host_under_a_foreign_operation_lock_still_attaches() {
        let (_refhosts_dir, config) = config_with_path_refhosts();
        let host = "template-named-locked.invalid";
        let (mut s, buf) = locking_session(config, &[host], &[]);
        s.inject_mock_connection(host, locked_host(host, op_lock_line("otheruser", 99, 60)));

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        assert_eq!(s.targets().names(), vec![host.to_owned()]);
        assert!(buf.contents().is_empty(), "{:?}", buf.contents());
    }

    /// A host the template names stays named when its testplatform draw also
    /// selects it: it attaches under a foreign operation lock and serves that
    /// slot itself, so no sibling is drawn for it.
    #[tokio::test]
    async fn template_named_host_also_drawn_attaches_under_a_foreign_lock() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let store = Session::build_refhosts_store(&config)
            .await
            .expect("fixture inventory");
        let (_, slots, _) = Session::pool_select(
            &store,
            &[tp.to_owned()],
            test_arbiter(),
            &("probe".to_owned(), "SUSE:Maintenance:9:9".to_owned()),
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;
        let candidates: Vec<String> = slots.values().next().expect("a slot").clone();
        assert!(
            candidates.len() >= 2,
            "a slot needs a sibling: {candidates:?}"
        );
        let named = candidates[0].clone();
        let lock = op_lock_line("otheruser", 99, 60);
        let (mut s, buf) = locking_session(config, &[named.as_str()], &[tp]);
        s.shuffle = no_shuffle;
        let mut named_conn = None;
        for h in slots.values().flatten() {
            let conn = product_host(h, "SLES", "15", "5", false);
            let conn = if *h == named {
                let c = conn.with_file(mtui_hosts::TARGET_LOCK_PATH, lock.clone());
                named_conn = Some(c.clone());
                c
            } else {
                conn
            };
            s.inject_mock_connection(h, conn);
        }
        let named_conn = named_conn.expect("the named host is a candidate");

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        let names = s.targets().names();
        assert!(names.contains(&named), "{names:?}");
        for sibling in &candidates[1..] {
            assert!(!names.contains(sibling), "{sibling} drawn: {names:?}");
        }
        assert!(
            s.metadata().base().pool_claims.contains(&named),
            "the draw selected the named host, or this test proves nothing"
        );
        assert_eq!(
            named_conn.file_contents(mtui_hosts::TARGET_LOCK_PATH),
            Some(lock)
        );
        let out = buf.contents();
        assert!(!out.contains("skipped"), "{out:?}");
    }

    /// `add_host` records the slot each host was drawn for on the report.
    #[tokio::test]
    async fn add_testplatform_hosts_records_the_slot_each_host_serves() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let store = Session::build_refhosts_store(&config)
            .await
            .expect("fixture inventory");
        let (_, slots, _) = Session::pool_select(
            &store,
            &[tp.to_owned()],
            test_arbiter(),
            &("probe".to_owned(), "SUSE:Maintenance:9:9".to_owned()),
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;
        let (slot, candidates) = slots.into_iter().next().expect("a slot");
        let (mut s, _buf) = locking_session(config, &[], &[tp]);
        s.shuffle = no_shuffle;
        for h in &candidates {
            s.inject_mock_connection(h, product_host(h, "SLES", "15", "5", false));
        }

        s.add_testplatform_hosts().await;

        assert_eq!(s.targets().names(), vec![candidates[0].clone()]);
        assert_eq!(
            s.metadata().base().pool_slots,
            std::collections::HashMap::from([(candidates[0].clone(), slot)])
        );
    }

    /// A template's named host offered as a backup sibling (bare `add_host`
    /// after it dropped out) attaches under a foreign operation lock too.
    #[tokio::test]
    async fn template_named_backup_sibling_attaches_under_a_foreign_lock() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let (ro, named) = ("ro-named-backup-primary.invalid", "named-backup.invalid");
        let lock = op_lock_line("otheruser", 99, 60);
        s.inject_mock_connection(ro, product_host(ro, "SLES", "15", "6", true));
        s.inject_mock_connection(named, locked_host(named, lock));
        s.metadata_mut()
            .base_mut()
            .hostnames
            .insert(named.to_owned());
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[ro.to_owned(), named.to_owned()])],
            &[ro],
        );

        let (unserved, _failures) = s
            .connect_and_add_hosts(
                vec![ro.to_owned()],
                rrid,
                &std::collections::HashSet::from([ro.to_owned()]),
            )
            .await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![named.to_owned()]);
    }

    /// `load_template` names a pool host it skipped for a foreign lock, with
    /// the holder, when a sibling served its slot.
    #[tokio::test]
    async fn load_template_names_a_pool_host_skipped_for_a_foreign_lock() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let store = Session::build_refhosts_store(&config)
            .await
            .expect("fixture inventory");
        let (_, slots, _) = Session::pool_select(
            &store,
            &[tp.to_owned()],
            test_arbiter(),
            &("probe".to_owned(), "SUSE:Maintenance:9:9".to_owned()),
            &mut Default::default(),
            0,
            0,
            no_shuffle,
        )
        .await;
        let candidates: Vec<String> = slots.values().next().expect("a slot").clone();
        assert!(
            candidates.len() >= 2,
            "a slot needs a sibling: {candidates:?}"
        );
        let primary = candidates[0].clone();
        let (mut s, buf) = locking_session(config, &[], &[tp]);
        s.shuffle = no_shuffle;
        for h in slots.values().flatten() {
            let conn = product_host(h, "SLES", "15", "5", false);
            let conn = if *h == primary {
                conn.with_file(
                    mtui_hosts::TARGET_LOCK_PATH,
                    op_lock_line("otheruser", 99, 60),
                )
            } else {
                conn
            };
            s.inject_mock_connection(h, conn);
        }

        s.autoconnect_active("SUSE:Maintenance:1:1").await;

        let names = s.targets().names();
        assert!(!names.contains(&primary), "{names:?}");
        assert!(names.contains(&candidates[1]), "{names:?}");
        let out = buf.contents();
        assert!(
            out.contains(&format!(
                "skipped {primary}: operation lock held by otheruser since "
            )),
            "{out:?}"
        );
    }

    /// `add_host`'s pool draw names a refused primary even when a sibling
    /// serves its slot, so no unserved-slot line carries it.
    #[tokio::test]
    async fn add_host_names_a_refused_primary_whose_slot_a_sibling_served() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let candidates = resolve_testplatform_hosts(&config, &[tp.to_owned()]).await;
        assert!(
            candidates.len() >= 2,
            "a slot needs a sibling: {candidates:?}"
        );
        let (mut s, buf) = locking_session(config, &[], &[tp]);
        s.shuffle = no_shuffle;
        let (rw, ro) = candidates.split_last().unwrap();
        for h in ro {
            s.inject_mock_connection(h, product_host(h, "SLES", "15", "5", true));
        }
        s.inject_mock_connection(rw, product_host(rw, "SLES", "15", "5", false));

        let unserved = s.add_testplatform_hosts().await;

        assert!(unserved.is_empty(), "{unserved:?}");
        assert_eq!(s.targets().names(), vec![rw.clone()]);
        let out = buf.contents();
        assert!(
            out.contains(&format!(
                "skipped {}: no command set for SLES 15-SP5, transactional",
                ro[0]
            )),
            "{out:?}"
        );
    }

    /// A refused host already named by its unserved slot is not named again.
    #[tokio::test]
    async fn add_host_names_a_refused_host_of_an_unserved_slot_once() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = 1;
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let candidates = resolve_testplatform_hosts(&config, &[tp.to_owned()]).await;
        let (mut s, buf) = locking_session(config, &[], &[tp]);
        for h in &candidates {
            s.inject_mock_connection(h, product_host(h, "SLES", "15", "5", true));
        }

        let unserved = s.add_testplatform_hosts().await;

        assert!(
            matches!(unserved.as_slice(), [Unserved::Unconnectable { .. }]),
            "{unserved:?}"
        );
        let out = buf.contents();
        assert!(!out.contains("skipped"), "{out:?}");
    }

    /// A refused host's product problem is named as such, and its
    /// refhosts.yml drift still reaches the display.
    #[tokio::test]
    async fn refused_host_names_its_product_problem_and_keeps_its_drift() {
        let (_refhosts_dir, config) = config_with_path_refhosts();
        let tp = "base=sles(major=15,minor=5);arch=[x86_64]";
        let inventoried = resolve_testplatform_hosts(&config, &[tp.to_owned()])
            .await
            .remove(0);
        let (mut s, buf) = locking_session(config, &[], &[]);
        // The link names a product file that is not there.
        let dangling = MockConnection::new(&inventoried)
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod");
        let leap = "leap.invalid";
        s.inject_mock_connection(&inventoried, dangling.clone());
        s.inject_mock_connection(
            leap,
            MockConnection::new(leap)
                .with_missing_dir("/etc/products.d")
                .with_file(
                    "/etc/os-release",
                    b"ID=\"opensuse-leap\"\nVERSION_ID=\"15.6\"\n".to_vec(),
                ),
        );

        let mut refused = s
            .add_named_hosts(vec![inventoried.clone(), leap.to_owned()])
            .await;
        refused.sort_by(|a, b| a.0.cmp(&b.0));

        assert!(s.targets().is_empty());
        assert!(!holds_a_lock(&dangling));
        let rendered: Vec<(String, String)> = refused
            .iter()
            .map(|(h, f)| (h.clone(), f.render()))
            .collect();
        let mut expected = vec![
            (
                inventoried.clone(),
                "base product unreadable (/etc/products.d/baseproduct is missing or dangling)"
                    .to_owned(),
            ),
            (
                leap.to_owned(),
                "unrecognised product opensuse-leap 15.6".to_owned(),
            ),
        ];
        expected.sort();
        assert_eq!(rendered, expected);
        let out = buf.contents();
        assert!(
            out.contains(&format!(
                "{inventoried}: products differ from refhosts.yml metadata:\n  - dangling /etc/products.d/baseproduct symlink"
            )),
            "{out:?}"
        );
        assert!(
            !s.metadata()
                .base()
                .product_warnings
                .contains_key(&inventoried),
            "a refused host's drift is shown, not recorded"
        );
    }

    /// With no update loaded a host without a command set still attaches:
    /// `run`, `get` and `put` serve it, and nothing will try to update it.
    #[tokio::test]
    async fn named_host_without_command_set_attaches_with_nothing_loaded() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let mut s = Session::new(config, false);
        assert!(s.metadata().id().is_empty(), "nothing must be loaded");
        let host = "ro-unloaded.invalid";
        s.inject_mock_connection(host, product_host(host, "SLES", "15", "6", true));

        let refused = s.add_named_hosts(vec![host.to_owned()]).await;

        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(s.targets().names(), vec![host.to_owned()]);
    }

    /// A host with `transactional-update.conf` whose mount table has no `/`
    /// entry, so its root mode is unknown.
    fn unknown_root_host(host: &str, name: &str, baseversion: &str) -> MockConnection {
        crate::commands::testkit::product_host(host, name, baseversion, "0", true)
            .with_file("/proc/self/mounts", "/dev/vda3 /home btrfs rw 0 0\n")
    }

    /// While an update is loaded, a SLES 16 host of unknown root mode is
    /// refused by that name, and an SL Micro one, which has no read-write
    /// variant, attaches.
    #[tokio::test]
    async fn unknown_root_sles16_is_refused_while_an_update_is_loaded() {
        let (_refhosts_dir, config) = config_with_path_refhosts();
        let mut s = Session::new(config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let (sles, micro) = ("sles16-unknown.invalid", "slmicro-unknown.invalid");
        s.inject_mock_connection(sles, unknown_root_host(sles, "SLES", "16.1"));
        s.inject_mock_connection(micro, unknown_root_host(micro, "SL-Micro", "6.1"));

        let refused = s
            .add_named_hosts(vec![sles.to_owned(), micro.to_owned()])
            .await;

        let reasons: Vec<_> = refused
            .iter()
            .map(|(h, f)| (h.as_str(), f.render()))
            .collect();
        assert_eq!(
            reasons,
            vec![(
                sles,
                "no command set for SLES 16.1, transactional-update.conf but root mode unknown"
                    .to_owned()
            )]
        );
        assert_eq!(s.targets().names(), vec![micro.to_owned()]);
    }

    /// With nothing loaded a SLES 16 host of unknown root mode attaches.
    #[tokio::test]
    async fn unknown_root_sles16_attaches_with_nothing_loaded() {
        let (_refhosts_dir, config) = config_with_path_refhosts();
        let mut s = Session::new(config, false);
        assert!(s.metadata().id().is_empty(), "nothing must be loaded");
        let host = "sles16-unknown-unloaded.invalid";
        s.inject_mock_connection(host, unknown_root_host(host, "SLES", "16.1"));

        let refused = s.add_named_hosts(vec![host.to_owned()]).await;

        assert!(refused.is_empty(), "{refused:?}");
        assert!(
            s.targets()
                .get(host)
                .is_some_and(|t| t.transactional_state() == mtui_hosts::Transactional::Assumed)
        );
    }

    /// Hosts whose key has a command set attach as before, a transactional SL
    /// Micro one included.
    #[tokio::test]
    async fn hosts_with_a_command_set_still_attach() {
        use crate::commands::testkit::product_host;

        let (_refhosts_dir, config) = config_with_path_refhosts();
        let mut s = Session::new(config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);
        let (sles, micro) = ("sles-rw.invalid", "slmicro.invalid");
        s.inject_mock_connection(sles, product_host(sles, "SLES", "15", "6", false));
        s.inject_mock_connection(micro, product_host(micro, "SL-Micro", "6", "1", true));

        let refused = s
            .add_named_hosts(vec![sles.to_owned(), micro.to_owned()])
            .await;

        assert!(refused.is_empty(), "{refused:?}");
        assert!(s.targets().get(sles).is_some_and(|t| !t.transactional()));
        assert!(s.targets().get(micro).is_some_and(Target::transactional));
    }

    /// A session over the active report with the pool seam wired, for the
    /// failed-primary tests below. Returns the temp dir holding the
    /// materialized refhosts inventory first: it must outlive the session,
    /// or the resolver silently degrades to the inventory-unavailable path.
    fn pool_session(
        connect_timeout: u64,
        lock_wait: u64,
    ) -> (
        tempfile::TempDir,
        Session,
        &'static HostArbiter,
        Owner,
        &'static str,
    ) {
        let (refhosts_dir, mut config) = config_with_path_refhosts();
        config.connect_timeout = connect_timeout;
        config.lock_wait = lock_wait;
        let mut s = Session::new(config, false);
        let rrid = "SUSE:Maintenance:1:1";
        seed_active_report(&mut s, rrid, &[], &[]);
        (
            refhosts_dir,
            s,
            test_arbiter(),
            ("reg".to_owned(), rrid.to_owned()),
            rrid,
        )
    }

    /// A failed primary's claim is released even when the retry budget is
    /// already spent at its slot, or the slot would stay "held" and every later
    /// `add_host` would skip it silently.
    #[tokio::test]
    async fn failed_primary_claim_is_released_when_budget_is_spent_at_slot_start() {
        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(0, 0);
        // The pool seam resolves testplatforms through the materialized
        // inventory: a dropped TempDir would silently degrade these tests to
        // the inventory-unavailable path.
        assert!(
            s.config.refhosts_path.is_file(),
            "pool_session must keep the refhosts fixture alive"
        );
        let sibling = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let primary = closed_addr();
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[primary.clone(), listener_addr(&sibling)])],
            &[&primary],
        );

        let unserved = s
            .connect_and_add_hosts(
                vec![primary.clone()],
                rrid,
                &std::collections::HashSet::from([primary.clone()]),
            )
            .await
            .0;

        assert_eq!(
            unserved,
            vec![Unserved::BudgetSpent {
                slot: "slot".to_owned(),
                tried: vec![primary.clone()],
            }]
        );
        assert_eq!(arbiter.owner_of(&primary), None);
        assert!(s.metadata().base().pool_claims.is_empty());
        assert!(!was_dialled(&sibling));
    }

    /// A slot another candidate already serves is left alone, but its dead
    /// primary must not keep a pool host pinned.
    #[tokio::test]
    async fn failed_primary_claim_is_released_when_slot_is_already_served() {
        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let spare = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let primary = closed_addr();
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[(
                "slot",
                &[primary.clone(), "mock-b".to_owned(), listener_addr(&spare)],
            )],
            &[&primary],
        );
        s.targets_mut().add(mock_target("mock-b"));

        let unserved = s
            .connect_and_add_hosts(
                vec![primary.clone()],
                rrid,
                &std::collections::HashSet::from([primary.clone()]),
            )
            .await
            .0;

        assert!(unserved.is_empty(), "a served slot reports nothing");
        assert_eq!(arbiter.owner_of(&primary), None);
        assert!(!s.metadata().base().pool_claims.contains(&primary));
        assert!(!was_dialled(&spare), "a served slot must not be backed up");
    }

    /// A named host that happens to be a slot candidate is not a pool claim, so
    /// its failure does not back the slot up.
    #[tokio::test]
    async fn named_slot_candidate_failure_does_not_back_up() {
        let (_refhosts_dir, mut s, arbiter, owner, _) = pool_session(1, 0);
        let sibling = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let named = closed_addr();
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[named.clone(), listener_addr(&sibling)])],
            &[],
        );

        s.add_named_hosts(vec![named]).await;

        assert!(!was_dialled(&sibling));
    }

    /// A budget that runs out between siblings is reported as such, not as
    /// every candidate being unconnectable.
    #[tokio::test]
    async fn budget_cut_inside_a_slots_sibling_walk_is_budget_spent() {
        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        // Black holes: each sibling costs a full `connect_timeout`, so the
        // fifth cannot start inside the 4s budget.
        let holes: Vec<std::net::TcpListener> = (0..5)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind"))
            .collect();
        let primary = closed_addr();
        let mut cands = vec![primary.clone()];
        cands.extend(holes.iter().map(listener_addr));
        wire_pool(&mut s, arbiter, &owner, &[("slot", &cands)], &[&primary]);

        let unserved = s
            .connect_and_add_hosts(
                vec![primary.clone()],
                rrid,
                &std::collections::HashSet::from([primary.clone()]),
            )
            .await
            .0;

        assert!(
            matches!(unserved.as_slice(), [Unserved::BudgetSpent { slot, .. }] if slot == "slot"),
            "{unserved:?}"
        );
        drop(holes);
    }

    /// Siblings the backup walk finds claimed by another RRID are named with it.
    #[tokio::test]
    async fn backup_names_the_claimant_of_a_busy_sibling() {
        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let primary = closed_addr();
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(arbiter.try_acquire("busy-host", &other));
        wire_pool(
            &mut s,
            arbiter,
            &owner,
            &[("slot", &[primary.clone(), "busy-host".to_owned()])],
            &[&primary],
        );

        let unserved = s
            .connect_and_add_hosts(
                vec![primary.clone()],
                rrid,
                &std::collections::HashSet::from([primary.clone()]),
            )
            .await
            .0;

        assert_eq!(
            unserved,
            vec![Unserved::Unconnectable {
                slot: "slot".to_owned(),
                failures: vec![
                    (primary, HostFailure::Unreachable),
                    (
                        "busy-host".to_owned(),
                        HostFailure::ClaimedBy(Some("SUSE:Maintenance:2:2".to_owned()))
                    ),
                ],
            }]
        );
    }

    /// Unserved slots come back in slot-key order, whatever order the map
    /// yields them in.
    #[tokio::test]
    async fn unserved_slots_are_reported_in_sorted_order() {
        let (_refhosts_dir, mut s, arbiter, owner, rrid) = pool_session(1, 0);
        let names: Vec<String> = (0..8).map(|i| format!("slot-{i}")).collect();
        let addrs = closed_addrs(16);
        let cands: Vec<[String; 2]> = addrs
            .chunks(2)
            .map(|pair| [pair[0].clone(), pair[1].clone()])
            .collect();
        let slots: Vec<(&str, &[String])> = names
            .iter()
            .zip(&cands)
            .map(|(n, c)| (n.as_str(), c.as_slice()))
            .collect();
        let primaries: Vec<String> = cands.iter().map(|c| c[0].clone()).collect();
        let claims: Vec<&str> = primaries.iter().map(String::as_str).collect();
        wire_pool(&mut s, arbiter, &owner, &slots, &claims);

        let unserved = s
            .connect_and_add_hosts(primaries.clone(), rrid, &primaries.into_iter().collect())
            .await
            .0;

        let got: Vec<&str> = unserved
            .iter()
            .map(|u| match u {
                Unserved::Unconnectable { slot, .. } => slot.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(got, names.iter().map(String::as_str).collect::<Vec<_>>());
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

        let (_refhosts_dir, refhosts_config) = config_with_path_refhosts();
        let mut s = Session::new(refhosts_config, false);
        seed_active_report(&mut s, "SUSE:Maintenance:1:1", &[], &[]);

        // The canonical session must not hold a guard on the entry the fork will
        // lock — the MCP `run_command` exclusive path releases it after each
        // call for exactly this reason.
        s.release_active_guard();

        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let mut fork = s.fork_for_call(display);
        assert_eq!(fork.display.color(), ColorMode::Always);
        assert!(fork.activate("SUSE:Maintenance:1:1").is_active());
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

    /// `fork_for_call` shares the `http_client` cache slot with the parent, so
    /// a stable posture builds exactly once no matter how many forks call it,
    /// and a posture change on the parent rebuilds exactly once more — the
    /// build count a per-fork cache could not produce (each fork would build
    /// its own).
    #[test]
    fn fork_for_call_shares_the_http_client_cache() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        use mtui_config::SslVerify;

        let mut s = Session::new(config(), false);
        let display = || CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);

        let fork1 = s.fork_for_call(display());
        let fork2 = s.fork_for_call(display());
        let _ = fork1.http_client().expect("fork1 builds");
        let _ = fork2.http_client().expect("fork2 reuses fork1's build");
        let _ = s.http_client().expect("parent reuses the same build");
        assert_eq!(s.http_builds(), 1, "one build shared across parent + forks");

        s.config.ssl_verify = SslVerify::Disabled;
        let fork3 = s.fork_for_call(display());
        let _ = fork3
            .http_client()
            .expect("fork3 rebuilds under new posture");
        let _ = s.http_client().expect("parent sees the rebuilt client");
        assert_eq!(s.http_builds(), 2, "posture change rebuilds exactly once");
    }

    #[test]
    fn fork_for_call_shares_the_op_lock_holders() {
        use crate::display::{ColorMode, CommandPromptDisplay};

        let s = Session::new(config(), false);
        let fork = s.fork_for_call(CommandPromptDisplay::with_sink(
            Box::new(Vec::new()),
            ColorMode::Never,
        ));
        assert!(fork.op_lock_holders().ptr_eq(s.op_lock_holders()));
        assert!(
            !Session::new(config(), false)
                .op_lock_holders()
                .ptr_eq(s.op_lock_holders()),
            "unrelated sessions must not share a registry"
        );
    }

    #[tokio::test]
    async fn activate_installs_the_op_lock_holders_on_the_active_groups_targets() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        assert!(s.activate("SUSE:Maintenance:1:1").is_active());

        assert!(
            s.targets()
                .get("t1")
                .unwrap()
                .op_lock_holders()
                .ptr_eq(s.op_lock_holders())
        );
    }

    #[tokio::test]
    async fn activate_installs_the_op_lock_holders_on_the_null_groups_targets() {
        let mut s = Session::new(config(), false);
        s.targets_mut().add(mock_target("n1"));
        assert_eq!(s.activate(""), Activation::Empty);

        assert!(
            s.targets()
                .get("n1")
                .unwrap()
                .op_lock_holders()
                .ptr_eq(s.op_lock_holders())
        );
    }

    #[tokio::test]
    async fn set_op_lock_holders_reaches_every_group_it_can_lock() {
        let mut s = Session::new(config(), false);
        seed_report_with_host(&mut s, "SUSE:Maintenance:1:1", "t1");
        seed_report_with_host(&mut s, "SUSE:Maintenance:2:2", "t2");
        s.targets_mut().add(mock_target("n1"));
        s.release_active_guard();

        let shared = OpLockHolders::default();
        s.set_op_lock_holders(shared.clone());

        assert!(s.op_lock_holders().ptr_eq(&shared));
        assert!(
            s.targets()
                .get("n1")
                .unwrap()
                .op_lock_holders()
                .ptr_eq(&shared)
        );
        for (rrid, host) in [
            ("SUSE:Maintenance:1:1", "t1"),
            ("SUSE:Maintenance:2:2", "t2"),
        ] {
            let on_shared = s
                .with_report(rrid, |r| {
                    r.base()
                        .targets
                        .get(host)
                        .unwrap()
                        .op_lock_holders()
                        .ptr_eq(&shared)
                })
                .unwrap();
            assert!(on_shared, "{host} stayed on its own registry");
        }
    }

    /// `fork_for_call` shares the `openqa_transport` cache slot with the
    /// parent, mirroring [`fork_for_call_shares_the_http_client_cache`].
    #[test]
    fn fork_for_call_shares_the_openqa_transport_cache() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        use mtui_config::SslVerify;

        let mut s = Session::new(config(), false);
        let display = || CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);

        let fork1 = s.fork_for_call(display());
        let fork2 = s.fork_for_call(display());
        let _ = fork1.openqa_transport().expect("fork1 builds");
        let _ = fork2
            .openqa_transport()
            .expect("fork2 reuses fork1's build");
        let _ = s.openqa_transport().expect("parent reuses the same build");
        assert_eq!(
            s.openqa_builds(),
            1,
            "one build shared across parent + forks"
        );

        s.config.ssl_verify = SslVerify::Disabled;
        let fork3 = s.fork_for_call(display());
        let _ = fork3
            .openqa_transport()
            .expect("fork3 rebuilds under new posture");
        let _ = s
            .openqa_transport()
            .expect("parent sees the rebuilt client");
        assert_eq!(s.openqa_builds(), 2, "posture change rebuilds exactly once");
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

    /// A stand-in for `connect_one`: records its start and in-flight count, then
    /// blocks on its gate until the controller releases it.
    struct WindowProbe {
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        starts: tokio::sync::mpsc::UnboundedSender<usize>,
    }

    async fn gated_host<T>(
        idx: usize,
        probe: Arc<WindowProbe>,
        gate: tokio::sync::oneshot::Receiver<()>,
        out: T,
    ) -> T {
        use std::sync::atomic::Ordering::SeqCst;
        let now = probe.active.fetch_add(1, SeqCst) + 1;
        probe.peak.fetch_max(now, SeqCst);
        let _ = probe.starts.send(idx);
        let _ = gate.await;
        probe.active.fetch_sub(1, SeqCst);
        out
    }

    type WindowGates = Vec<tokio::sync::oneshot::Sender<()>>;
    type WindowStarts = tokio::sync::mpsc::UnboundedReceiver<usize>;

    fn window_fixture<T>(
        outs: Vec<T>,
    ) -> (
        Arc<WindowProbe>,
        WindowStarts,
        WindowGates,
        Vec<impl Future<Output = T>>,
    ) {
        let (starts_tx, starts_rx) = tokio::sync::mpsc::unbounded_channel();
        let probe = Arc::new(WindowProbe {
            active: 0.into(),
            peak: 0.into(),
            starts: starts_tx,
        });
        let mut gates = Vec::new();
        let futs = outs
            .into_iter()
            .enumerate()
            .map(|(idx, out)| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                gates.push(tx);
                gated_host(idx, probe.clone(), rx, out)
            })
            .collect();
        (probe, starts_rx, gates, futs)
    }

    /// Waits up to a virtual second for the next start event.
    async fn next_start(starts: &mut WindowStarts) -> Option<usize> {
        tokio::time::timeout(Duration::from_secs(1), starts.recv())
            .await
            .ok()
            .flatten()
    }

    /// With a slow head host blocked, a free slot is refilled the moment a
    /// sibling completes rather than waiting for the whole first batch.
    #[tokio::test(start_paused = true)]
    async fn connect_window_refills_while_first_host_is_blocked() {
        let (_probe, mut starts, mut gates, futs) = window_fixture(vec!["A", "B", "C"]);
        let (gate_c, gate_b, gate_a) = (
            gates.pop().expect("c"),
            gates.pop().expect("b"),
            gates.pop().expect("a"),
        );
        let controller = async {
            let first = (next_start(&mut starts).await, next_start(&mut starts).await);
            // B is released; A stays blocked while we look for C.
            let _ = gate_b.send(());
            let c_started = next_start(&mut starts).await;
            let _ = gate_a.send(());
            let _ = gate_c.send(());
            (first, c_started)
        };
        let (out, (first, c_started)) = tokio::join!(Session::connect_bounded(futs, 2), controller);

        assert_eq!(first, (Some(0), Some(1)), "A and B start under bound 2");
        assert_eq!(
            c_started,
            Some(2),
            "C did not start within 1s of virtual time while A was still blocked"
        );
        assert_eq!(out, ["A", "B", "C"]);
    }

    /// In-flight work never exceeds the bound (0 behaves as 1), each future
    /// runs exactly once, and a freed slot is refilled at once.
    #[tokio::test(start_paused = true)]
    async fn connect_window_never_exceeds_bound() {
        for (bound, cap) in [(2usize, 2usize), (1, 1), (0, 1)] {
            let n = 5;
            let (probe, mut starts, gates, futs) = window_fixture((0..n).collect());
            let controller = async {
                let mut seen = Vec::new();
                let mut problems = Vec::new();
                for _ in 0..cap {
                    seen.extend(next_start(&mut starts).await);
                }
                if tokio::time::timeout(Duration::from_millis(10), starts.recv())
                    .await
                    .is_ok()
                {
                    problems.push("a start beyond the bound before any release".to_owned());
                }
                for (i, gate) in gates.into_iter().enumerate() {
                    let _ = gate.send(());
                    if seen.len() < n {
                        match next_start(&mut starts).await {
                            Some(idx) => seen.push(idx),
                            None => problems.push(format!("no refill after releasing host {i}")),
                        }
                    }
                }
                (seen, problems)
            };
            let (out, (seen, problems)) =
                tokio::join!(Session::connect_bounded(futs, bound), controller);

            assert!(problems.is_empty(), "bound {bound}: {problems:?}");
            assert_eq!(
                seen,
                (0..n).collect::<Vec<_>>(),
                "bound {bound}: start order"
            );
            assert_eq!(
                probe.peak.load(std::sync::atomic::Ordering::SeqCst),
                cap,
                "bound {bound}: peak in-flight"
            );
            assert_eq!(out, (0..n).collect::<Vec<_>>(), "bound {bound}: results");
        }
    }

    /// Outputs come back in input order whatever the completion order, so a
    /// duplicate host key still resolves to the later input.
    #[tokio::test(start_paused = true)]
    async fn connect_window_returns_input_order() {
        let outs = vec![("h", Ok(1)), ("g", Err("down")), ("h", Ok(3))];
        let (_probe, mut starts, mut gates, futs) = window_fixture(outs.clone());
        let (gate_c, gate_b, gate_a) = (
            gates.pop().expect("c"),
            gates.pop().expect("b"),
            gates.pop().expect("a"),
        );
        let controller = async {
            for _ in 0..3 {
                next_start(&mut starts).await;
            }
            for gate in [gate_c, gate_b, gate_a] {
                let _ = gate.send(());
                tokio::task::yield_now().await;
            }
        };
        let (out, ()) = tokio::join!(Session::connect_bounded(futs, 3), controller);

        assert_eq!(out, outs);
        let folded: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(folded["h"], Ok(3), "later input wins for a duplicate key");
    }
}
