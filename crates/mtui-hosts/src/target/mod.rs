//! The [`Target`] state machine — one reference host driven over a
//! [`Connection`].
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/target.py` (`Target`). Upstream's
//! `Target` is a god-class that also owns remote locks, `zypper`/repo
//! management, package version querying, system/product parsing, reboot and
//! reconnect lifecycle, a reporter, and the update-workflow doer/check
//! dispatch. This module deliberately ports only the **state machine over
//! `Connection`** (P2.4):
//!
//! * the per-host execution [`TargetState`] gate (enabled / dryrun / disabled),
//! * command execution via [`Target::run`] with the upstream `-1` exit-code
//!   sentinel and never-propagate error handling,
//! * the `last*` output accessors,
//! * state-gated SFTP delegation ([`Target::sftp_put`] / [`Target::sftp_get`]),
//! * and [`Target::connect`], which establishes the transport and then mirrors
//!   upstream's post-connect ordering: system/product parse
//!   ([`parse_system`](parsers::parse_system)) and package version query
//!   ([`Target::query_versions`]).
//!
//! The remaining upstream responsibilities that this module does *not* own are
//! reached through object-safe seams, keeping `mtui-hosts` acyclic:
//!
//! * remote locks — the [`locks`] module provides the zypper op-lock
//!   ([`TargetLock`]) and the pool-claim lock ([`PoolLock`]);
//!   [`Target::connect`] builds both objects over the live connection and runs
//!   upstream's connect-time op-lock check + stale-reap
//!   ([`check_stale_lock`](Target::check_stale_lock)),
//! * reboot / reconnect lifecycle and the install/uninstall
//!   [`Operation`] template drive their group through the object-safe
//!   [`OperationGroup`] seam; the concrete `impl OperationGroup for HostsGroup`
//!   binding lives in [`hostgroup`] (resolving each target's doer/check via the
//!   injected [`PlanProvider`]),
//! * repo management — [`RepoManager`] forwards `set`/`unset` through the
//!   object-safe [`SetRepo`] seam, whose concrete report impls live in
//!   `mtui-testreport`.
//!
//! Routing this machinery through seams preserves the acyclic crate graph
//! (`mtui-hosts` must not depend on `mtui-testreport`) and lets the whole
//! state machine be unit-tested offline against a `MockConnection`.

pub mod actions;
pub mod arbiter;
pub mod hostgroup;
pub mod locks;
pub mod operation;
pub mod package_querier;
pub mod parsers;
pub mod repo_manager;
pub mod reporter;
pub mod spinner;

pub use actions::{Command, RunCommand, run_parallel, sftp_get_all, sftp_put_all, sftp_remove_all};
pub use arbiter::{HostArbiter, Owner, get_arbiter};
pub use hostgroup::HostsGroup;
pub use locks::{
    Clock, LockRow, LockSnapshot, Lockable, POOL_LOCK_PATH, PoolLock, RemoteLock, SystemClock,
    TARGET_LOCK_PATH, TargetLock, with_locked,
};
pub use operation::{
    Check, CheckArgs, Doer, HostPlan, InstallOperation, LastOutput, Operation, OperationGroup,
    PlanProvider, UninstallOperation,
};
pub use package_querier::PackageQuerier;
pub use parsers::{parse_os_release, parse_product, parse_system};
pub use repo_manager::{RepoManager, RepoOp, SetRepo};
pub use reporter::Reporter;
pub use spinner::{
    Sink, SpinnerGuard, Suspend, SuspendAsync, TtySpinner, set_test_sink, spinner, suspend,
    suspend_async,
};

use std::path::{Path, PathBuf};

use mtui_config::Config;
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::hostlog::{CommandLog, HostLog};
use mtui_types::package::Package;
use mtui_types::system::{System, SystemProduct};

#[cfg(feature = "shell")]
use crate::connection::ShellChannel;
use crate::connection::{CommandTimeout, Connection, HostKeyPolicy, SshConnection};
use crate::error::{HostError, Result};

/// The dryrun stdout marker appended for every command echoed in
/// [`TargetState::Dryrun`], byte-identical to upstream's `"dryrun\n"`.
const DRYRUN_MARKER: &str = "dryrun\n";

/// A single reference host and its execution state.
///
/// The `Target` owns at most one [`Connection`] (a `Box<dyn Connection>` so the
/// russh [`SshConnection`] and the test [`MockConnection`] are
/// interchangeable) plus an ordered [`HostLog`] of everything run against it.
/// Commands are gated by [`TargetState`]:
///
/// * [`Enabled`](TargetState::Enabled) — run for real and record the outcome.
/// * [`Dryrun`](TargetState::Dryrun) — echo the command, record `"dryrun\n"`.
/// * [`Disabled`](TargetState::Disabled) — record an empty entry, touch nothing.
///
/// [`MockConnection`]: crate::MockConnection
pub struct Target {
    /// The full `host[:port]` string as supplied by the caller.
    hostname: String,
    /// The host part (everything before the first `:`), mirroring upstream's
    /// `hostname.partition(":")`.
    host: String,
    /// The port part (everything after the first `:`), or empty when none was
    /// given. Kept as a string to match upstream's `""`-means-default contract.
    port: String,
    /// Per-host execution state.
    state: TargetState,
    /// Whether this host runs in parallel with its group or under a serial
    /// barrier (consumed by the P2.5 fan-out).
    mode: ExecutionMode,
    /// Connect/command timeout, defaulted from
    /// [`Config::connection_timeout`](mtui_config::Config).
    timeout: CommandTimeout,
    /// The host-key policy resolved from config, used when [`connect`] builds
    /// the transport.
    ///
    /// [`connect`]: Target::connect
    policy: HostKeyPolicy,
    /// The ordered command log for this host.
    out: HostLog,
    /// The live connection, or `None` until [`connect`](Target::connect) (or a
    /// test injection) supplies one.
    connection: Option<Box<dyn Connection>>,
    /// The parsed host system (base product + addons). Defaults to an unknown
    /// system until [`parse_system`](parsers::parse_system) populates it via
    /// [`set_system`](Target::set_system); [`PackageQuerier`] reads it to choose
    /// the rpm-vs-dpkg query path.
    system: System,
    /// Whether the host is a transactional (read-only-root) system, set
    /// alongside [`system`](Self::system).
    transactional: bool,
    /// The config this target was built from, retained so
    /// [`connect`](Target::connect) can build the operation [`TargetLock`] with
    /// the session identity / reap / wait settings (upstream keeps `self.config`
    /// for the same reason).
    config: Config,
    /// The operation lock (`/var/lock/mtui.lock`), built in
    /// [`connect`](Target::connect) from a clone of this target's connection
    /// (upstream `self._lock = TargetLock(self.connection, self.config)`).
    /// `None` until connected. Drives [`unlock`](Target::unlock) and the
    /// [`RepoManager`] unknown-cmd force-unlock safeguard.
    lock: Option<TargetLock>,
    /// The pool-claim lock (`/var/lock/mtui-pool.lock`), built in
    /// [`connect`](Target::connect) / [`with_connection`](Target::with_connection)
    /// from a clone of this target's connection and seeded with [`rrid`](Self::rrid)
    /// (upstream `self._pool_lock = PoolLock(self.connection, self.config,
    /// self._rrid)`). `None` until connected. Drives
    /// [`pool_unlock`](Target::pool_unlock).
    pool_lock: Option<PoolLock>,
    /// The owning template's RRID, used as the [`PoolLock`] ownership identity
    /// (upstream `Target._rrid`). Empty for directly-constructed reports that
    /// never use pool selection; the report layer pushes it down via
    /// [`set_rrid`](Self::set_rrid).
    rrid: String,
    /// The per-host packages tracked across an update (upstream
    /// `Target.packages`, a `dict[str, Package]`). Each entry carries the
    /// metadata-`required` version and, once [`query_versions`](Target::query_versions)
    /// runs, the installed `current` version; `perform_update`'s package check
    /// records `before`/`after` here. Kept as an ordered `Vec` for deterministic
    /// iteration; seeded via [`set_packages`](Target::set_packages).
    packages: Vec<Package>,
    /// Optional interactive command-timeout prompt, applied to the transport in
    /// [`connect`](Target::connect) via
    /// [`SshConnection::with_timeout_prompt`](crate::connection::SshConnection::with_timeout_prompt).
    /// `None` (the default, and always headless / under `mtui-mcp`) keeps a
    /// no-output timeout an immediate abort; the composition root installs a
    /// [`Prompter`](crate::prompter::Prompter)-backed prompt via
    /// [`set_timeout_prompt`](Target::set_timeout_prompt) for the REPL.
    timeout_prompt: Option<crate::connection::TimeoutPrompt>,
}

/// Builds the placeholder [`System`] a freshly-constructed [`Target`] carries
/// before it has been parsed — an `unknown` base with no addons, matching the
/// upstream `Target.system` sentinel prior to `_parse_system`.
fn unknown_system() -> System {
    System::new(
        SystemProduct::new("unknown", "", ""),
        Default::default(),
        false,
    )
}

impl Target {
    /// Builds an **unconnected** target from config, matching upstream
    /// `Target.__init__` defaults.
    ///
    /// The `hostname` is split on the first `:` into `host`/`port` exactly like
    /// upstream's `partition(":")`: `"h.example:2222"` yields host `h.example`
    /// and port `2222`; a bare `"h.example"` yields an empty port. The timeout
    /// and host-key policy are taken from `config`. Call
    /// [`connect`](Target::connect) to establish the transport.
    #[must_use]
    pub fn new(
        config: &Config,
        hostname: impl Into<String>,
        state: TargetState,
        mode: ExecutionMode,
    ) -> Self {
        let hostname = hostname.into();
        let (host, port) = split_host_port(&hostname);
        Self {
            hostname,
            host,
            port,
            state,
            mode,
            timeout: CommandTimeout::from_secs(config.connection_timeout),
            policy: HostKeyPolicy::from_config(&config.ssh_strict_host_key_checking),
            out: HostLog::new(),
            connection: None,
            system: unknown_system(),
            transactional: false,
            config: config.clone(),
            lock: None,
            pool_lock: None,
            rrid: String::new(),
            packages: Vec::new(),
            timeout_prompt: None,
        }
    }

    /// Builds a target around an already-established [`Connection`].
    ///
    /// This is the offline test seam: inject a
    /// [`MockConnection`](crate::MockConnection) so the whole state machine can
    /// be exercised without a live host. The timeout and policy carry defaults;
    /// they are unused when a connection is pre-supplied.
    #[must_use]
    pub fn with_connection(
        hostname: impl Into<String>,
        state: TargetState,
        mode: ExecutionMode,
        connection: Box<dyn Connection>,
    ) -> Self {
        let hostname = hostname.into();
        let (host, port) = split_host_port(&hostname);
        let config = Config::default();
        // Build the operation lock from a clone of the injected connection so
        // the test seam mirrors the connected state: `unlock` / the RepoManager
        // force-unlock safeguard have a live lock without a `connect()` call.
        // The clone shares the mock's scripted state (`Arc`), so a lock SFTP op
        // is observable through the original handle.
        let lock = Some(TargetLock::new(connection.clone_box(), &config));
        // Build the pool-claim lock from a clone of the injected connection too,
        // so the test seam mirrors the connected state (a fresh target has no
        // RRID yet — the report layer pushes it down via `set_rrid`).
        let pool_lock = Some(PoolLock::new(
            connection.clone_box(),
            &config,
            String::new(),
        ));
        Self {
            hostname,
            host,
            port,
            state,
            mode,
            timeout: CommandTimeout::default(),
            policy: HostKeyPolicy::default(),
            out: HostLog::new(),
            connection: Some(connection),
            system: unknown_system(),
            transactional: false,
            config,
            lock,
            pool_lock,
            rrid: String::new(),
            packages: Vec::new(),
            timeout_prompt: None,
        }
    }

    /// The full `host[:port]` string this target was constructed with.
    #[must_use]
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// The host part (before the first `:`).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port part (after the first `:`), or `""` when none was given.
    #[must_use]
    pub fn port(&self) -> &str {
        &self.port
    }

    /// The current per-host execution state.
    #[must_use]
    pub const fn state(&self) -> TargetState {
        self.state
    }

    /// Sets the per-host execution state (the REPL `hoststate` command).
    pub fn set_state(&mut self, state: TargetState) {
        self.state = state;
    }

    /// The execution mode (parallel vs serial barrier) for group fan-out.
    #[must_use]
    pub const fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Sets the per-host execution mode (the REPL `set_host_state`
    /// `serial`/`parallel` variants).
    ///
    /// The mode counterpart to [`set_state`](Self::set_state): upstream
    /// `HostState` sets `target.mode` for the `serial`/`parallel` choices and
    /// `target.state` for the enabled/disabled/dryrun choices.
    pub fn set_mode(&mut self, mode: ExecutionMode) {
        self.mode = mode;
    }

    /// Whether a live connection is currently attached.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.as_ref().is_active())
    }

    /// Read-only access to the command log.
    #[must_use]
    pub fn out(&self) -> &HostLog {
        &self.out
    }

    /// The parsed host system (base product + addons).
    ///
    /// Returns the `unknown` placeholder until
    /// [`set_system`](Self::set_system) records a parsed one.
    #[must_use]
    pub fn system(&self) -> &System {
        &self.system
    }

    /// Whether the host is a transactional (read-only-root) system.
    #[must_use]
    pub const fn transactional(&self) -> bool {
        self.transactional
    }

    /// The connect/command timeout for this target, in whole seconds.
    ///
    /// Upstream stores the timeout on the connection (`connection.timeout`);
    /// the Rust `Target` owns it directly (defaulted from
    /// [`Config::connection_timeout`](mtui_config::Config)). Exposed for the
    /// [`Reporter::timeout`](reporter::Reporter::timeout) sink.
    #[must_use]
    pub const fn timeout_secs(&self) -> u64 {
        self.timeout.as_secs()
    }

    /// Sets the connect/command timeout for this target, in whole seconds.
    ///
    /// Ports upstream `Target.set_timeout` (which sets `connection.timeout`);
    /// the Rust `Target` owns the timeout directly, so this updates the field
    /// that later [`connect`](Self::connect) calls apply and that
    /// [`timeout_secs`](Self::timeout_secs) reports. `0` disables the timeout
    /// (upstream semantics), which [`CommandTimeout`] represents as a zero
    /// duration.
    pub const fn set_timeout(&mut self, secs: u64) {
        self.timeout = CommandTimeout::from_secs(secs);
    }

    /// Returns a [`Reporter`] bound to this target for status-sink dispatch.
    ///
    /// The reporter borrows `self`, so each dispatch reads live field values —
    /// the Rust analogue of upstream's per-access `Target.reporter` property.
    #[must_use]
    pub fn reporter(&self) -> Reporter<'_> {
        Reporter::new(self)
    }

    /// Returns a [`RepoManager`] bound to this target for zypper-repo lifecycle.
    ///
    /// Unlike [`reporter`](Self::reporter), the repo manager borrows `self`
    /// *mutably* — it issues commands and, on the unknown-cmd safeguard, force-
    /// unlocks the target. Like upstream's per-access `Target.repo_manager`
    /// property it hands out a fresh binding over the live target each call.
    #[must_use]
    pub fn repo_manager(&mut self) -> RepoManager<'_> {
        RepoManager::new(self)
    }

    /// Releases this target's operation lock, best-effort.
    ///
    /// Ports upstream `Target.unlock`: delegates to the operation
    /// [`TargetLock::unlock`], swallowing a [`HostError::TargetLocked`] (the
    /// lock is held by another owner and `force` was not set) so a cleanup path
    /// never fails the caller — upstream wraps the call in
    /// `suppress(TargetLockedError)`. With `force = true` a foreign lock is
    /// removed anyway (the [`RepoManager`] unknown-cmd safeguard uses this).
    ///
    /// A no-op when the target is not connected (no lock built yet).
    pub async fn unlock(&mut self, force: bool) {
        let Some(lock) = self.lock.as_mut() else {
            tracing::debug!(host = %self.hostname, "unlock: no lock (not connected)");
            return;
        };
        match lock.unlock(force).await {
            Ok(()) => {}
            Err(HostError::TargetLocked(msg)) => {
                tracing::debug!(host = %self.hostname, %msg, "unlock: lock held by another owner, ignoring");
            }
            Err(e) => {
                tracing::warn!(host = %self.hostname, error = %e, "unlock failed");
            }
        }
    }

    /// Sets the owning template's RRID, the [`PoolLock`] ownership identity.
    ///
    /// The report layer pushes the RRID down onto each target (see
    /// [`HostsGroup::set_rrid`]) so a pool claim already built for a connected
    /// target adopts the identity too. Upstream sets `Target._rrid` at
    /// construction; here it is pushed down after the fact because the target is
    /// built before the report that owns it is known.
    pub fn set_rrid(&mut self, rrid: impl Into<String>) {
        self.rrid = rrid.into();
        if let Some(pool) = self.pool_lock.as_mut() {
            pool.set_rrid(self.rrid.clone());
        }
    }

    /// The owning template's RRID (empty when unset).
    #[must_use]
    pub fn rrid(&self) -> &str {
        &self.rrid
    }

    /// Releases this target's pool claim, best-effort.
    ///
    /// Ports upstream `Target.pool_unlock`: delegates to [`PoolLock::unlock`]
    /// (RRID-based ownership), swallowing a [`HostError::TargetLocked`] (the
    /// claim is owned by another template and `force` was not set) so a cleanup
    /// path never fails. A no-op when the target is not connected (no pool lock
    /// built yet).
    pub async fn pool_unlock(&mut self, force: bool) {
        let Some(pool) = self.pool_lock.as_mut() else {
            tracing::debug!(host = %self.hostname, "pool_unlock: no pool lock (not connected)");
            return;
        };
        match pool.unlock(force).await {
            Ok(()) => {}
            Err(HostError::TargetLocked(msg)) => {
                tracing::debug!(host = %self.hostname, %msg, "pool_unlock: claim held by another template, ignoring");
            }
            Err(e) => {
                tracing::warn!(host = %self.hostname, error = %e, "pool_unlock failed");
            }
        }
    }

    /// Claims this target's remote pool lock with `comment` (the
    /// `mtui pool <RRID> [<owner>]` stamp), returning whether the claim was won.
    ///
    /// Ports upstream `Target.try_claim`: delegates to [`PoolLock::try_claim`]
    /// (RRID-based ownership). `true` when this session now holds the remote
    /// claim (freshly won or already ours); `false` when another process holds
    /// it — the caller then drops the host so a sibling can be tried. A
    /// not-connected target (no pool lock built) returns `false`.
    ///
    /// # Errors
    ///
    /// Propagates any I/O error from writing/reading the remote lock file.
    pub async fn pool_claim(&mut self, comment: &str) -> Result<bool> {
        let Some(pool) = self.pool_lock.as_mut() else {
            tracing::debug!(host = %self.hostname, "pool_claim: no pool lock (not connected)");
            return Ok(false);
        };
        pool.try_claim(comment).await
    }

    /// Acquires this target's operation lock with `comment`.
    ///
    /// Ports upstream `Target.lock`: delegates to
    /// [`TargetLock::lock`]. A no-op when the target is not connected (no lock
    /// built yet), matching the group fan-out's tolerance of unconnected hosts.
    ///
    /// # Errors
    ///
    /// Propagates [`HostError::TargetLocked`] when the lock is held by another
    /// owner (callers that want best-effort behaviour suppress it, as the group
    /// [`lock`](HostsGroup::lock) fan-out does).
    pub async fn lock(&mut self, comment: &str) -> Result<()> {
        let Some(lock) = self.lock.as_mut() else {
            tracing::debug!(host = %self.hostname, "lock: no lock (not connected)");
            return Ok(());
        };
        lock.lock(comment).await
    }

    /// Reports whether this target's operation lock is currently held.
    ///
    /// Ports upstream `Target.is_locked`: delegates to
    /// [`TargetLock::is_locked`]. Returns `false` when the target is not
    /// connected (no lock built yet).
    ///
    /// # Errors
    ///
    /// Propagates any transport error raised while reading the remote lock file.
    pub async fn is_locked(&mut self) -> Result<bool> {
        match self.lock.as_mut() {
            Some(lock) => lock.is_locked().await,
            None => Ok(false),
        }
    }

    /// Returns this target's operation lock, loaded, for inspecting ownership.
    ///
    /// The group [`update_lock`](HostsGroup::update_lock) fan-out needs to read
    /// [`is_mine`](TargetLock::is_mine) / [`time`](TargetLock::time) /
    /// [`locked_by`](TargetLock::locked_by) / [`comment`](TargetLock::comment)
    /// after establishing the host is locked; exposing the built lock mirrors
    /// upstream reaching into `t._lock`. `None` when not connected.
    pub(crate) fn lock_mut(&mut self) -> Option<&mut TargetLock> {
        self.lock.as_mut()
    }

    /// Resolves this target's current lock ownership into a [`LockRow`].
    ///
    /// Ports the read side of upstream `Reporter.locks` / `Reporter.pool_locks`:
    /// loads the operation lock (or the pool-claim lock when `pool` is `true`),
    /// then reads `is_mine` / `time` / `locked_by` / `comment` — the same
    /// resolution [`update_lock`](HostsGroup::update_lock) performs — and returns
    /// the already-resolved (sync) values so the display layer stays sync. An
    /// unconnected target (no built lock) resolves to the empty, unlocked row.
    ///
    /// Best-effort like upstream: a read error on an individual accessor
    /// degrades to its default rather than aborting the whole `list_locks`
    /// fan-out.
    pub async fn lock_status(&mut self, pool: bool) -> LockRow {
        if pool {
            let Some(lock) = self.pool_lock.as_mut() else {
                return LockRow::default();
            };
            // Single remote read; every field is derived from the snapshot.
            let Ok(snap) = lock.snapshot().await else {
                return LockRow::default();
            };
            if snap.lock.user.is_empty() {
                return LockRow::default();
            }
            let time = snap.lock.display_time();
            LockRow {
                is_locked: true,
                is_mine: snap.is_mine,
                locked_by: snap.lock.user,
                time,
                // A pool claim's detail is the owning template's RRID (parsed
                // from the `mtui pool <RRID> [<owner>]` stamp), not the raw
                // comment the operation lock carries.
                comment: snap.rrid,
            }
        } else {
            let Some(lock) = self.lock.as_mut() else {
                return LockRow::default();
            };
            // Single remote read; every field is derived from the snapshot.
            let Ok(snap) = lock.snapshot().await else {
                return LockRow::default();
            };
            if snap.lock.user.is_empty() {
                return LockRow::default();
            }
            let time = snap.lock.display_time();
            LockRow {
                is_locked: true,
                is_mine: snap.is_mine,
                locked_by: snap.lock.user,
                time,
                comment: snap.lock.comment,
            }
        }
    }

    /// Reads the host's current boot id.
    ///
    /// Ports upstream `Target.boot_id`: runs
    /// `cat /proc/sys/kernel/random/boot_id` (regenerated on every boot) and
    /// returns the trimmed stdout, or `""` if the value cannot be read (no
    /// connection, command failure, or empty output). Used by the group reboot
    /// lifecycle to confirm a host actually rebooted.
    pub async fn boot_id(&mut self) -> String {
        let Some(conn) = self.connection.as_mut() else {
            tracing::debug!(host = %self.hostname, "boot_id: not connected");
            return String::new();
        };
        match conn.run("cat /proc/sys/kernel/random/boot_id").await {
            Ok(log) => log.stdout.trim().to_owned(),
            Err(e) => {
                tracing::debug!(host = %self.hostname, error = %e, "boot_id: read failed");
                String::new()
            }
        }
    }

    /// Sends `command` without waiting for it to return.
    ///
    /// Ports upstream `Target.reboot`: dispatches via
    /// [`Connection::fire_and_forget`]. The command is expected to drop the SSH
    /// connection, so callers follow up with [`reconnect`](Self::reconnect). A
    /// no-op (logged) when the target is not connected; a dispatch error is
    /// logged, not propagated, since a link dropped by the reboot is expected.
    pub async fn reboot(&mut self, command: &str) {
        let Some(conn) = self.connection.as_mut() else {
            tracing::error!(host = %self.hostname, "reboot on unconnected target");
            return;
        };
        if let Err(e) = conn.fire_and_forget(command).await {
            tracing::error!(host = %self.hostname, error = %e, "failed to dispatch reboot");
        }
    }

    /// Re-establishes the transport after a reboot dropped it.
    ///
    /// Ports upstream `Target.reconnect(retry, backoff)`: delegates to
    /// [`Connection::reconnect`], which encapsulates the bounded retry +
    /// backoff loop upstream passes explicitly. A no-op when the target is not
    /// connected.
    ///
    /// # Errors
    ///
    /// Propagates [`HostError::ReconnectFailed`] when the retry budget is
    /// exhausted while the host is still down.
    pub async fn reconnect(&mut self) -> Result<()> {
        match self.connection.as_mut() {
            Some(conn) => conn.reconnect().await,
            None => {
                tracing::debug!(host = %self.hostname, "reconnect: not connected");
                Ok(())
            }
        }
    }

    /// Cleanly disconnects the target, optionally rebooting or powering it off.
    ///
    /// Ports upstream `Target.close(action)`. When a live connection exists it
    /// is first quiesced — best-effort [`unlock`](Self::unlock) of the operation
    /// lock and [`pool_unlock`](Self::pool_unlock) of the pool claim (both
    /// `force = false`, so locks/claims owned by another owner are left intact) —
    /// then `action` selects the disconnect behaviour:
    ///
    /// * `Some("reboot")` → dispatch `reboot` (fire-and-forget; the link drops),
    /// * `Some("poweroff")` → dispatch `halt` (upstream maps `poweroff` to the
    ///   shell `halt`; fire-and-forget),
    /// * `None` (or any other value) → just close the connection.
    ///
    /// The local connection is closed last regardless. A no-op when the target
    /// is not connected. Unlike [`HostsGroup::reboot`](crate::HostsGroup::reboot)
    /// this never reconnects — it is the teardown used on session exit.
    ///
    /// Returns the final connection-shutdown outcome so callers (`quit` via
    /// [`HostsGroup::close`](crate::HostsGroup::close)) can name a host that
    /// failed to disconnect. The lock/claim release above stays best-effort and
    /// is *not* folded into the result: a lock held by another owner (or a lost
    /// link during unlock) must not mask the shutdown outcome.
    pub async fn close(&mut self, action: Option<&str>) -> Result<()> {
        let active = self.connection.as_ref().is_some_and(|c| c.is_active());
        if active {
            // Best-effort release of our own locks before disconnecting; a lock
            // held by another owner is left untouched (force = false).
            self.unlock(false).await;
            self.pool_unlock(false).await;
        } else {
            tracing::debug!(host = %self.hostname, "close: not connected");
        }

        match action {
            Some("reboot") => {
                tracing::info!(host = %self.hostname, "rebooting");
                self.reboot("reboot").await;
            }
            Some("poweroff") => {
                tracing::info!(host = %self.hostname, "powering off");
                self.reboot("halt").await;
            }
            _ => {
                tracing::info!(host = %self.hostname, "closing connection");
            }
        }

        if let Some(conn) = self.connection.as_mut() {
            conn.close().await?;
        }
        Ok(())
    }

    /// Records the parsed host system and its transactional flag.
    ///
    /// Called with the pair returned by
    /// [`parse_system`](parsers::parse_system) once a connection is live; the
    /// two values always move together (upstream sets `self.system` and
    /// `self.transactional` in the same `_parse_system` step).
    pub fn set_system(&mut self, system: System, transactional: bool) {
        self.system = system;
        self.transactional = transactional;
    }

    /// Re-parses the host's system/product over the live connection and records
    /// the result.
    ///
    /// Ports upstream `Target.reload_system` (driven by the `reload_products`
    /// command): re-runs [`parse_system`](parsers::parse_system) and stores the
    /// `(System, transactional)` pair via [`set_system`](Self::set_system). A
    /// no-op (logged) when the target is not connected; a parse failure is
    /// logged and the previously recorded system is left untouched, matching the
    /// crate's best-effort degradation elsewhere.
    pub async fn reload_system(&mut self) {
        let Some(conn) = self.connection.as_mut() else {
            tracing::debug!(host = %self.hostname, "reload_system: not connected");
            return;
        };
        match parsers::parse_system(conn.as_mut()).await {
            Ok((system, transactional)) => self.set_system(system, transactional),
            Err(e) => {
                tracing::warn!(host = %self.hostname, error = %e, "reload_system: parse failed");
            }
        }
    }

    /// The per-host tracked packages (upstream `Target.packages`).
    #[must_use]
    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    /// Mutable access to the tracked packages, so an update flow can record
    /// `before`/`after` versions across its two [`query_versions`](Target::query_versions)
    /// passes (upstream mutates `t.packages[...]` in `package_check`).
    pub fn packages_mut(&mut self) -> &mut [Package] {
        &mut self.packages
    }

    /// Seeds the tracked packages (each carrying its metadata-`required`
    /// version).
    ///
    /// The Rust analogue of upstream `Target._parse_packages`, which builds
    /// `Target.packages` from the report's metadata. Here the report supplies
    /// the already-resolved [`Package`] list (name + `required`) at the start of
    /// an update flow, keeping the metadata/product resolution in
    /// `mtui-testreport` and off this crate.
    pub fn set_packages(&mut self, packages: Vec<Package>) {
        self.packages = packages;
    }

    /// Installs the interactive command-timeout prompt applied by the next
    /// [`connect`](Target::connect).
    ///
    /// The composition root (`mtui-cli`) wires a
    /// [`Prompter`](crate::prompter::Prompter)-backed prompt here for the REPL so
    /// a stuck command asks the user whether to keep waiting; headless callers
    /// (`mtui-mcp`) leave it unset and a timeout aborts immediately.
    pub fn set_timeout_prompt(&mut self, prompt: crate::connection::TimeoutPrompt) {
        self.timeout_prompt = Some(prompt);
    }

    /// Whether an interactive command-timeout prompt is installed (test seam for
    /// the [`HostsGroup::set_prompter`](crate::HostsGroup::set_prompter) /
    /// [`Session`] push-down).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn has_timeout_prompt(&self) -> bool {
        self.timeout_prompt.is_some()
    }

    /// Queries the installed versions of the tracked packages and records them
    /// as each package's `current` version.
    ///
    /// Ports upstream `Target.query_versions()` (the no-argument form): runs the
    /// [`PackageQuerier`] over `self.packages` and sets `current` from the
    /// result (`None` when the package is not installed). A no-op when no
    /// packages are tracked. Only meaningful for an
    /// [`Enabled`](TargetState::Enabled) host; other states record nothing, as
    /// their `run` does not touch a live connection.
    pub async fn query_versions(&mut self) {
        let names: Vec<String> = self.packages.iter().map(|p| p.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let versions = PackageQuerier::new(self).versions(&names).await;
        for pkg in &mut self.packages {
            pkg.set_current_version(versions.get(&pkg.name).cloned().flatten());
        }
    }

    /// Appends a history entry to the remote `/var/log/mtui.log`.
    ///
    /// Ports upstream `Target.add_history`: on **enabled** hosts only, writes one
    /// `timestamp:user:field1:field2…\n` line (Unix-epoch-seconds timestamp,
    /// `config.session_user`, then the colon-joined `fields`). The upstream
    /// wire-format contract is shared with the Python mtui and read back by
    /// `list_history`, so it is preserved byte-for-byte.
    ///
    /// Upstream opens the file `"a+"` and writes one line; this sends only that
    /// new line via [`sftp_append`](Connection::sftp_append), which appends at
    /// end-of-file and creates the file if it is missing. Unlike the former
    /// read-concatenate-rewrite emulation, the cost per entry is now O(1) in the
    /// line size (not O(history)), and concurrent writers (a Rust and a Python
    /// mtui sharing the host) no longer clobber one another's entries.
    ///
    /// Best-effort, matching upstream: a write failure (read-only or full remote
    /// fs, unconnected host) is logged and swallowed so a bookkeeping write never
    /// aborts the operation it records.
    pub async fn add_history(&mut self, fields: &[String]) {
        if self.state != TargetState::Enabled {
            return;
        }
        let hostname = self.hostname.clone();
        let user = self.config.session_user.clone();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!("{now}:{user}:{}\n", fields.join(":"));

        let Some(conn) = self.connection.as_mut() else {
            tracing::error!(host = %hostname, "add_history on unconnected target");
            return;
        };
        let path = std::path::Path::new("/var/log/mtui.log");
        if let Err(e) = conn.sftp_append(path, line.as_bytes()).await {
            tracing::warn!(host = %hostname, error = %e, "failed to write history entry");
        }
    }

    /// Runs upstream's connect-time lock check + stale-reap on the operation
    /// lock.
    ///
    /// Ports `Target.connect`'s `if self.is_locked() and not
    /// self._lock.reap_if_stale(): logger.warning(self._lock.locked_by_msg())`
    /// (`target.py:187-188`): if the host is locked by a prior session, reap it
    /// when it is old enough ([`TargetLock::reap_if_stale`]), otherwise warn who
    /// holds it. Operates on the operation lock only, so it is independent of the
    /// session RRID (that is the pool lock's concern).
    ///
    /// Best-effort like upstream's surrounding degradation: a transport error
    /// while reading the remote lock is logged and swallowed rather than failing
    /// the connect — an unreadable lock must not block the host.
    async fn check_stale_lock(&mut self) {
        let Some(lock) = self.lock.as_mut() else {
            return;
        };
        match lock.is_locked().await {
            Ok(false) => {}
            Ok(true) => match lock.reap_if_stale().await {
                Ok(true) => {} // reaped; `reap_if_stale` already warned.
                Ok(false) => match lock.locked_by_msg().await {
                    Ok(msg) => tracing::warn!("{msg}"),
                    Err(e) => tracing::warn!(
                        host = %self.hostname, error = %e,
                        "connect: reading lock owner failed"
                    ),
                },
                Err(e) => tracing::warn!(
                    host = %self.hostname, error = %e,
                    "connect: stale-lock reap failed"
                ),
            },
            Err(e) => tracing::warn!(
                host = %self.hostname, error = %e,
                "connect: lock check failed"
            ),
        }
    }

    /// Establishes the SSH transport for this target and parses its system.
    ///
    /// Ports upstream `Target.connect`: builds an [`SshConnection`] from the
    /// host/port/timeout/policy resolved at construction, builds the two
    /// remote locks over it, runs the connect-time lock check + stale-reap
    /// ([`check_stale_lock`](Target::check_stale_lock)), then — mirroring
    /// upstream's post-dial ordering — parses the system/product
    /// ([`parse_system`](parsers::parse_system)) and queries installed package
    /// versions ([`Target::query_versions`]). If a connection is already
    /// attached (e.g. a test-injected
    /// [`MockConnection`](crate::MockConnection)), the dial + lock-building
    /// step is skipped, but the check/parse/query steps still run — this is what
    /// lets the whole flow be unit-tested against a scripted `MockConnection`
    /// without a live socket.
    ///
    /// System parsing is retried a few times on a transient SFTP failure (a
    /// connect-time timeout that would otherwise strand the host at the
    /// `unknown` sentinel); if it still fails after the retry budget, `connect()`
    /// returns the error so the caller drops the host — mirroring upstream, which
    /// does not swallow a `parse_system` failure. (This differs from
    /// [`reload_system`](Target::reload_system), which degrades to the sentinel
    /// because the host is already a live group member there.) The
    /// reboot/reconnect/operation lifecycle is bound at the composition root —
    /// see the module docs.
    ///
    /// # Errors
    ///
    /// Propagates [`HostError::Connect`] / [`HostError::Auth`] from
    /// [`SshConnection::connect`] when the host is unreachable or auth fails, and
    /// the last [`parse_system`](parsers::parse_system) error when the system
    /// cannot be parsed after the retry budget is exhausted.
    pub async fn connect(&mut self) -> Result<()> {
        if self.connection.is_none() {
            tracing::info!(host = %self.hostname, "connecting");
            let port = self.port.parse::<u16>().unwrap_or(0);
            let conn = SshConnection::connect(
                self.host.clone(),
                port,
                self.policy,
                self.timeout,
                None,
            )
            .await
            .inspect_err(|e| {
                tracing::error!(host = %self.hostname, error = %e, "connecting to target failed");
            })?;
            // Wire the interactive command-timeout prompt when the composition
            // root supplied one (REPL); headless targets leave it unset
            // (immediate abort).
            let conn = match &self.timeout_prompt {
                Some(prompt) => conn.with_timeout_prompt(prompt.clone()),
                None => conn,
            };
            let conn: Box<dyn Connection> = Box::new(conn);
            // Build the operation lock over a clone of the live connection,
            // mirroring upstream `self._lock = TargetLock(self.connection,
            // self.config)`. The lock uses this handle for its SFTP-based lock
            // protocol and force-unlock.
            self.lock = Some(TargetLock::new(conn.clone_box(), &self.config));
            // Pool claims use a separate remote file + RRID-based ownership,
            // mirroring upstream `self._pool_lock = PoolLock(self.connection,
            // self.config, self._rrid)`.
            self.pool_lock = Some(PoolLock::new(
                conn.clone_box(),
                &self.config,
                self.rrid.clone(),
            ));
            self.connection = Some(conn);
        } else {
            tracing::debug!(host = %self.hostname, "already connected; skipping re-dial");
        }

        // Mirror upstream `connect()` line 187-188: check the operation lock and
        // reap it if stale, otherwise warn who holds it. Runs whether the
        // connection was just dialed or already attached, matching the
        // parse/query steps below.
        self.check_stale_lock().await;

        // Mirror upstream `connect()`'s post-dial ordering: `self.system,
        // self.transactional = parse_system(self.connection)` then
        // `self.query_versions()`. Runs whether the connection was just dialed
        // or already attached, so a short-circuited re-`connect()` still picks
        // up the real system instead of leaving the `unknown` sentinel.
        //
        // `parse_system` already resolves the non-SUSE branch internally
        // (`SftpNotFound` on `/etc/products.d` → `/etc/os-release` fallback), so
        // a propagated `Err` here is an *unexpected* failure — in practice a
        // transient SFTP timeout on a freshly-dialed session. A single such
        // timeout used to leave the host permanently `unknown--` for the rest of
        // the session (never seeded, `list_packages` shows blanks, refhosts
        // drift spuriously warns), even though SSH itself is fine. Retry a few
        // times with a short backoff so a transient stall self-heals.
        //
        // Upstream `connect()` does NOT wrap `parse_system` in try/except: a
        // parse failure propagates out of `connect()` and the caller drops the
        // host. Mirror that — a host we cannot parse (after retries) is unusable
        // (no system ⇒ no seed, no doer/repo resolution), so surface the error
        // rather than keep an `unknown--` zombie in the group.
        if let Some(conn) = self.connection.as_mut() {
            const PARSE_RETRIES: u32 = 3;
            let mut attempt = 0;
            loop {
                match parsers::parse_system(conn.as_mut()).await {
                    Ok((system, transactional)) => {
                        self.set_system(system, transactional);
                        break;
                    }
                    Err(e) if attempt < PARSE_RETRIES => {
                        attempt += 1;
                        tracing::warn!(
                            host = %self.hostname, error = %e, attempt,
                            "connect: system parse failed; retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => {
                        tracing::error!(
                            host = %self.hostname, error = %e, attempts = attempt + 1,
                            "connect: system parse failed after retries; dropping host"
                        );
                        return Err(e);
                    }
                }
            }
        }
        self.query_versions().await;

        Ok(())
    }

    /// Runs `command` on the host, gated by [`TargetState`].
    ///
    /// * [`Enabled`](TargetState::Enabled): delegates to
    ///   [`Connection::run`] and records the resulting [`CommandLog`]. A
    ///   [`HostError::Timeout`] is caught and recorded with the upstream `-1`
    ///   exit-code sentinel; **any** other connection error is logged and
    ///   likewise recorded as `-1` — never propagated, mirroring upstream's
    ///   catch-all so one bad host never aborts a group fan-out.
    /// * [`Dryrun`](TargetState::Dryrun): records `command` with `"dryrun\n"`
    ///   stdout and exit `0`, without touching the connection.
    /// * [`Disabled`](TargetState::Disabled): records an empty entry and does
    ///   nothing else.
    pub async fn run(&mut self, command: &str) {
        match self.state {
            TargetState::Enabled => {
                tracing::debug!(host = %self.hostname, %command, "running");
                let log = match self.connection.as_mut() {
                    Some(conn) => match conn.run(command).await {
                        Ok(log) => log,
                        Err(HostError::Timeout { .. }) => {
                            tracing::error!(host = %self.hostname, %command, "command timed out");
                            CommandLog::new(command, "", "", -1, 0)
                        }
                        Err(e) => {
                            tracing::error!(
                                host = %self.hostname, %command, error = %e,
                                "failed to run command"
                            );
                            CommandLog::new(command, "", "", -1, 0)
                        }
                    },
                    None => {
                        tracing::error!(
                            host = %self.hostname, %command,
                            "run on unconnected target"
                        );
                        CommandLog::new(command, "", "", -1, 0)
                    }
                };
                self.out.push(log);
            }
            TargetState::Dryrun => {
                tracing::info!(host = %self.hostname, %command, "dryrun");
                self.out
                    .push(CommandLog::new(command, DRYRUN_MARKER, "", 0, 0));
            }
            TargetState::Disabled => {
                self.out.push(CommandLog::new("", "", "", 0, 0));
            }
        }
    }

    /// The last command that was run, or `""` when the log is empty.
    #[must_use]
    pub fn lastin(&self) -> &str {
        self.out.last().map_or("", |e| e.command.as_str())
    }

    /// The last command's stdout, or `""` when the log is empty.
    #[must_use]
    pub fn lastout(&self) -> &str {
        self.out.last().map_or("", |e| e.stdout.as_str())
    }

    /// The last command's stderr, or `""` when the log is empty.
    #[must_use]
    pub fn lasterr(&self) -> &str {
        self.out.last().map_or("", |e| e.stderr.as_str())
    }

    /// The last command's exit code, or `None` when the log is empty.
    ///
    /// Upstream returns `int | str` (the code, or `""` when empty); modelling
    /// the empty case as `None` keeps the type honest — no serialized contract
    /// depends on the empty-string form.
    #[must_use]
    pub fn lastexit(&self) -> Option<i16> {
        self.out.last().map(|e| e.exitcode)
    }

    /// Uploads a local file to the host over SFTP, gated by [`TargetState`].
    ///
    /// [`Enabled`](TargetState::Enabled) delegates to
    /// [`Connection::sftp_put`]; a transfer error is logged, not propagated
    /// (upstream behaviour). [`Dryrun`](TargetState::Dryrun) logs the intended
    /// transfer; [`Disabled`](TargetState::Disabled) does nothing.
    pub async fn sftp_put(&mut self, local: &Path, remote: &Path) {
        match self.state {
            TargetState::Enabled => {
                let Some(conn) = self.connection.as_mut() else {
                    tracing::error!(host = %self.hostname, "sftp_put on unconnected target");
                    return;
                };
                if let Err(e) = conn.sftp_put(local, remote).await {
                    tracing::error!(
                        host = %self.hostname, local = %local.display(), error = %e,
                        "failed to send"
                    );
                }
            }
            TargetState::Dryrun => {
                tracing::info!(
                    host = %self.hostname,
                    "dryrun: put {} {}:{}",
                    local.display(), self.hostname, remote.display()
                );
            }
            TargetState::Disabled => {}
        }
    }

    /// Uploads already-read bytes to `remote` over SFTP, gated by
    /// [`TargetState`].
    ///
    /// The read-once counterpart of [`sftp_put`](Self::sftp_put): a fan-out
    /// reads the local payload a single time and dispatches the shared bytes to
    /// every host. Same failure semantics (logged, not propagated).
    pub async fn sftp_put_bytes(&mut self, data: &[u8], remote: &Path) {
        match self.state {
            TargetState::Enabled => {
                let Some(conn) = self.connection.as_mut() else {
                    tracing::error!(host = %self.hostname, "sftp_put on unconnected target");
                    return;
                };
                if let Err(e) = conn.sftp_put_bytes(data, remote).await {
                    tracing::error!(
                        host = %self.hostname, remote = %remote.display(), error = %e,
                        "failed to send"
                    );
                }
            }
            TargetState::Dryrun => {
                tracing::info!(
                    host = %self.hostname,
                    "dryrun: put <{} bytes> {}:{}",
                    data.len(), self.hostname, remote.display()
                );
            }
            TargetState::Disabled => {}
        }
    }

    /// Downloads a file (or folder) from the host over SFTP, gated by
    /// [`TargetState`].
    ///
    /// The file-vs-folder branch mirrors upstream exactly: a `remote` path with
    /// a trailing `/` fetches a folder via [`Connection::sftp_get_folder`] into
    /// `local/`; otherwise a single file is fetched via
    /// [`Connection::sftp_get`] into `local.{hostname}` — the per-host suffix is
    /// a workflow contract so a parallel fan-out never clobbers one local dir.
    /// A transfer error is logged, not propagated.
    pub async fn sftp_get(&mut self, remote: &str, local: &Path) {
        let is_folder = remote.ends_with('/');
        let remote_path = PathBuf::from(remote);
        let local_target = if is_folder {
            PathBuf::from(format!("{}/", local.display()))
        } else {
            PathBuf::from(format!("{}.{}", local.display(), self.hostname))
        };

        match self.state {
            TargetState::Enabled => {
                let Some(conn) = self.connection.as_mut() else {
                    tracing::error!(host = %self.hostname, "sftp_get on unconnected target");
                    return;
                };
                let res = if is_folder {
                    conn.sftp_get_folder(&remote_path, &local_target).await
                } else {
                    conn.sftp_get(&remote_path, &local_target).await
                };
                if let Err(e) = res {
                    tracing::error!(
                        host = %self.hostname, remote = %remote, error = %e,
                        "failed to get"
                    );
                }
            }
            TargetState::Dryrun => {
                tracing::info!(
                    host = %self.hostname,
                    "dryrun: get {}:{} {}",
                    self.hostname, remote, local_target.display()
                );
            }
            TargetState::Disabled => {}
        }
    }

    /// Deletes a file (or directory) on the host over SFTP, gated by
    /// [`TargetState`].
    ///
    /// Mirrors upstream `Target.sftp_remove`: a plain file remove is attempted
    /// first via [`Connection::sftp_remove`]; if that fails the path may be a
    /// directory, so [`Connection::sftp_rmdir`] is tried as a fallback. A
    /// missing path or a failed fallback is logged, never propagated (upstream
    /// behaviour). [`Dryrun`](TargetState::Dryrun) logs the intended removal;
    /// [`Disabled`](TargetState::Disabled) does nothing.
    pub async fn sftp_remove(&mut self, path: &Path) {
        match self.state {
            TargetState::Enabled => {
                let Some(conn) = self.connection.as_mut() else {
                    tracing::error!(host = %self.hostname, "sftp_remove on unconnected target");
                    return;
                };
                if conn.sftp_remove(path).await.is_err() {
                    // The path may be a directory rather than a file; fall back
                    // to rmdir before giving up, matching upstream's OSError
                    // recovery branch.
                    if let Err(e) = conn.sftp_rmdir(path).await {
                        tracing::warn!(
                            host = %self.hostname, path = %path.display(), error = %e,
                            "unable to remove"
                        );
                    }
                }
            }
            TargetState::Dryrun => {
                tracing::info!(
                    host = %self.hostname,
                    "dryrun: remove {}:{}",
                    self.hostname, path.display()
                );
            }
            TargetState::Disabled => {}
        }
    }

    /// Opens an interactive PTY shell on the host, gated by [`TargetState`].
    ///
    /// Mirrors upstream `Target.shell`: on an [`Enabled`](TargetState::Enabled)
    /// target it delegates to [`Connection::shell`] and returns the
    /// [`ShellChannel`]; a spawn failure is logged and swallowed as `None`
    /// (upstream's `except Exception: log "failed to spawn shell"`), so one bad
    /// host never aborts a sequential fan-out. [`Dryrun`](TargetState::Dryrun)
    /// and [`Disabled`](TargetState::Disabled) do nothing and return `None`
    /// (there is no PTY to spawn under a dry run).
    ///
    /// The returned handle is a transport duplex only; the raw-`termios` local
    /// terminal bridge that consumes it is a CLI concern (Phase 6).
    ///
    /// Available only with the `shell` feature.
    #[cfg(feature = "shell")]
    pub async fn shell(&mut self, cols: u32, rows: u32) -> Option<Box<dyn ShellChannel>> {
        match self.state {
            TargetState::Enabled => {
                tracing::debug!(host = %self.hostname, "spawning shell");
                let Some(conn) = self.connection.as_mut() else {
                    tracing::error!(host = %self.hostname, "shell on unconnected target");
                    return None;
                };
                match conn.shell(cols, rows).await {
                    Ok(channel) => Some(channel),
                    Err(e) => {
                        tracing::error!(
                            host = %self.hostname, error = %e,
                            "failed to spawn shell"
                        );
                        None
                    }
                }
            }
            TargetState::Dryrun => {
                tracing::info!(host = %self.hostname, "dryrun: shell");
                None
            }
            TargetState::Disabled => None,
        }
    }
}

/// Splits `host[:port]` into `(host, port)` on the first `:`, matching upstream
/// `hostname.partition(":")`. A missing port yields an empty string.
fn split_host_port(hostname: &str) -> (String, String) {
    match hostname.split_once(':') {
        Some((h, p)) => (h.to_owned(), p.to_owned()),
        None => (hostname.to_owned(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::MockConnection;
    use crate::connection::MockSftpOp;

    fn cfg() -> Config {
        Config::default()
    }

    fn enabled_with(conn: MockConnection) -> Target {
        Target::with_connection(
            "test-host.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        )
    }

    // --- construction / defaults -------------------------------------------

    #[test]
    fn new_uses_upstream_defaults() {
        let t = Target::new(
            &cfg(),
            "test-host.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        assert_eq!(t.hostname(), "test-host.example.com");
        assert_eq!(t.host(), "test-host.example.com");
        assert_eq!(t.port(), "");
        assert_eq!(t.state(), TargetState::Enabled);
        assert_eq!(t.mode(), ExecutionMode::Parallel);
        assert!(!t.is_connected());
        assert!(t.out().is_empty());
    }

    #[test]
    fn new_splits_host_and_port() {
        let t = Target::new(
            &cfg(),
            "test-host.example.com:2222",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        assert_eq!(t.host(), "test-host.example.com");
        assert_eq!(t.port(), "2222");
        assert_eq!(t.hostname(), "test-host.example.com:2222");
    }

    #[test]
    fn timeout_defaults_from_config() {
        let mut c = cfg();
        c.connection_timeout = 600;
        let t = Target::new(
            &c,
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        assert_eq!(t.timeout, CommandTimeout::from_secs(600));
    }

    #[test]
    fn set_timeout_updates_reported_seconds() {
        let mut t = Target::new(
            &cfg(),
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        t.set_timeout(120);
        assert_eq!(t.timeout_secs(), 120);
        // `0` disables the timeout (upstream semantics): zero duration.
        t.set_timeout(0);
        assert_eq!(t.timeout_secs(), 0);
    }

    #[test]
    fn set_state_switches_gate() {
        let mut t = Target::new(
            &cfg(),
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        t.set_state(TargetState::Disabled);
        assert_eq!(t.state(), TargetState::Disabled);
    }

    #[test]
    fn set_mode_switches_execution_mode() {
        let mut t = Target::new(
            &cfg(),
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        t.set_mode(ExecutionMode::Serial);
        assert_eq!(t.mode(), ExecutionMode::Serial);
        t.set_mode(ExecutionMode::Parallel);
        assert_eq!(t.mode(), ExecutionMode::Parallel);
    }

    #[tokio::test]
    async fn reload_system_reparses_over_live_connection() {
        // A SUSE host whose product XML parses to SLES 15-SP5.
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec());
        let mut t = enabled_with(conn);
        // Starts with the `unknown` placeholder before any parse.
        assert_eq!(t.system().get_base().name, "unknown");

        t.reload_system().await;

        assert_eq!(t.system().get_base().name, "SLES");
        assert_eq!(t.system().get_base().version, "15-SP5");
    }

    #[tokio::test]
    async fn reload_system_on_unconnected_is_noop() {
        let mut t = Target::new(
            &cfg(),
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        t.reload_system().await;
        // No connection -> system untouched (still the unknown placeholder).
        assert_eq!(t.system().get_base().name, "unknown");
    }

    // --- run() state machine ------------------------------------------------

    #[tokio::test]
    async fn run_enabled_executes_and_records() {
        let conn = MockConnection::new("test-host.example.com").with_response(
            "echo hello",
            CommandLog::new("echo hello", "output", "", 0, 3),
        );
        let mut t = enabled_with(conn);

        t.run("echo hello").await;

        assert_eq!(t.lastin(), "echo hello");
        assert_eq!(t.lastout(), "output");
        assert_eq!(t.lastexit(), Some(0));
    }

    #[tokio::test]
    async fn run_dryrun_does_not_execute() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Dryrun,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.run("rm -rf /").await;

        assert!(
            handle.commands().is_empty(),
            "dryrun must not issue commands"
        );
        assert_eq!(t.lastin(), "rm -rf /");
        assert_eq!(t.lastout(), DRYRUN_MARKER);
        assert_eq!(t.lastexit(), Some(0));
    }

    #[tokio::test]
    async fn run_disabled_does_not_execute() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Disabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.run("some command").await;

        assert!(
            handle.commands().is_empty(),
            "disabled must not issue commands"
        );
        assert_eq!(t.lastin(), "");
        assert_eq!(t.lastout(), "");
        assert_eq!(t.lastexit(), Some(0));
    }

    #[tokio::test]
    async fn run_timeout_yields_exit_minus_one() {
        let conn = MockConnection::new("h1").with_timeout("sleep 999");
        let mut t = enabled_with(conn);

        t.run("sleep 999").await;

        assert_eq!(t.lastexit(), Some(-1));
        assert_eq!(t.lastin(), "sleep 999");
    }

    #[tokio::test]
    async fn run_on_unconnected_target_records_minus_one() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);

        t.run("echo hi").await;

        assert_eq!(t.lastexit(), Some(-1));
    }

    // --- last* accessors ----------------------------------------------------

    #[test]
    fn last_methods_empty_log() {
        let t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        assert_eq!(t.lastin(), "");
        assert_eq!(t.lastout(), "");
        assert_eq!(t.lasterr(), "");
        assert_eq!(t.lastexit(), None);
    }

    #[tokio::test]
    async fn last_methods_with_output() {
        let conn = MockConnection::new("h1").with_response(
            "ls -la",
            CommandLog::new("ls -la", "file1\nfile2\n", "warning\n", 0, 5),
        );
        let mut t = enabled_with(conn);

        t.run("ls -la").await;

        assert_eq!(t.lastin(), "ls -la");
        assert!(t.lastout().contains("file1"));
        assert!(t.lasterr().contains("warning"));
        assert_eq!(t.lastexit(), Some(0));
    }

    // --- sftp delegation ----------------------------------------------------

    #[tokio::test]
    async fn sftp_put_enabled_delegates() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.sftp_put(Path::new("/local"), Path::new("/remote")).await;

        assert_eq!(
            handle.sftp_ops(),
            vec![MockSftpOp::Put {
                local: PathBuf::from("/local"),
                remote: PathBuf::from("/remote"),
            }]
        );
    }

    #[tokio::test]
    async fn sftp_put_dryrun_does_nothing() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Dryrun,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.sftp_put(Path::new("/local"), Path::new("/remote")).await;

        assert!(handle.sftp_ops().is_empty());
    }

    #[tokio::test]
    async fn sftp_get_file_uses_hostname_suffix() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.sftp_get("/remote/file", Path::new("/local/file")).await;

        assert_eq!(
            handle.sftp_ops(),
            vec![MockSftpOp::Get {
                remote: PathBuf::from("/remote/file"),
                // local suffixed with the full host[:port] hostname.
                local: PathBuf::from("/local/file.test-host.example.com"),
            }]
        );
    }

    #[tokio::test]
    async fn sftp_get_trailing_slash_uses_folder() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.sftp_get("/remote/dir/", Path::new("/local")).await;

        assert_eq!(
            handle.sftp_ops(),
            vec![MockSftpOp::GetFolder {
                remote: PathBuf::from("/remote/dir/"),
                local: PathBuf::from("/local/"),
            }]
        );
    }

    #[tokio::test]
    async fn sftp_get_dryrun_does_nothing() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Dryrun,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.sftp_get("/remote/file", Path::new("/local")).await;

        assert!(handle.sftp_ops().is_empty());
    }

    // --- sftp_remove --------------------------------------------------------

    #[tokio::test]
    async fn sftp_remove_enabled_removes_file() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.sftp_remove(Path::new("/remote/file")).await;

        assert_eq!(
            handle.sftp_ops(),
            vec![MockSftpOp::Remove(PathBuf::from("/remote/file"))]
        );
    }

    #[tokio::test]
    async fn sftp_remove_falls_back_to_rmdir_when_remove_fails() {
        // A failed file remove (e.g. the path is a directory) falls back to
        // rmdir, matching upstream's OSError recovery branch.
        let conn = MockConnection::new("h1").failing_sftp_remove();
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.sftp_remove(Path::new("/remote/dir")).await;

        assert_eq!(
            handle.sftp_ops(),
            vec![
                MockSftpOp::Remove(PathBuf::from("/remote/dir")),
                MockSftpOp::Rmdir(PathBuf::from("/remote/dir")),
            ]
        );
    }

    #[tokio::test]
    async fn sftp_remove_dryrun_does_nothing() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Dryrun,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.sftp_remove(Path::new("/remote/file")).await;

        assert!(handle.sftp_ops().is_empty());
    }

    #[tokio::test]
    async fn sftp_remove_disabled_does_nothing() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Disabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.sftp_remove(Path::new("/remote/file")).await;

        assert!(handle.sftp_ops().is_empty());
    }

    // --- connect() ----------------------------------------------------------

    #[tokio::test]
    async fn connect_is_noop_when_already_connected() {
        // A pre-injected (mock) connection means connect() skips the SSH
        // re-dial, but still attempts the post-connect system parse: a bare
        // mock has no `/etc/products.d` listing or `baseproduct` symlink
        // scripted, so `sftp_listdir` succeeds with an empty listing (SUSE
        // branch) but `resolve_basefile`'s `sftp_readlink` errors (not
        // `SftpNotFound`) and `parse_system` surfaces that — best-effort
        // degrade, `connect()` still returns `Ok` and leaves the `unknown`
        // sentinel (see `connect_parse_failure_leaves_sentinel` for the
        // matching malformed-XML case).
        let mut t = enabled_with(MockConnection::new("test-host.example.com"));
        assert!(t.is_connected());
        t.connect().await.expect("noop re-dial");
        assert!(t.is_connected());
        assert_eq!(t.system().get_base().name, "unknown");
    }

    #[tokio::test]
    async fn connect_parses_system_over_live_connection() {
        // Same SLES fixture as `reload_system_reparses_over_live_connection`,
        // but scripted through a short-circuited `connect()` (the only way to
        // reach the post-dial parse step without a live socket).
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec());
        let mut t = enabled_with(conn);
        assert_eq!(t.system().get_base().name, "unknown");

        t.connect().await.expect("connect");

        assert_eq!(t.system().get_base().name, "SLES");
        assert_eq!(t.system().get_base().version, "15-SP5");
    }

    #[tokio::test]
    async fn connect_errors_on_unrecoverable_parse_failure() {
        // A SUSE host whose base product file is present but has malformed XML:
        // `parse_system` surfaces the XML error on every attempt, so `connect()`
        // exhausts its retry budget and propagates the error — the caller drops
        // the host rather than keep an unusable `unknown--` zombie (mirrors
        // upstream, which never swallows a parse_system failure).
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file(
                "/etc/products.d/SLES.prod",
                b"<product><name>x</wrong></product>".to_vec(),
            );
        let mut t = enabled_with(conn);

        assert!(
            t.connect().await.is_err(),
            "an unparseable system must fail connect so the host is dropped"
        );
    }

    #[tokio::test]
    async fn connect_retries_transient_system_parse_failure() {
        // A flaky SFTP session: the first two `/etc/products.d` listdirs time
        // out, the third succeeds. `connect()`'s bounded retry must recover the
        // real system instead of leaving the host permanently `unknown--` (the
        // whale-31 field bug — a single connect-time SFTP timeout stranded the
        // host with no system, so it was never seeded and list_packages showed
        // blanks / spurious refhosts drift).
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>6</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec())
            .with_transient_listdir_failures("/etc/products.d", 2);
        let mut t = enabled_with(conn);
        assert_eq!(t.system().get_base().name, "unknown");

        t.connect().await.expect("connect");

        assert_eq!(t.system().get_base().name, "SLES");
        assert_eq!(t.system().get_base().version, "15-SP6");
    }

    #[tokio::test]
    async fn connect_errors_when_transient_failure_exceeds_budget() {
        // If the SFTP session keeps timing out past the retry budget (3),
        // `connect()` gives up and propagates the error so the caller drops the
        // host — it never returns a half-connected `unknown--` member.
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>6</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("h1")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec())
            .with_transient_listdir_failures("/etc/products.d", 99);
        let mut t = enabled_with(conn);

        assert!(
            t.connect().await.is_err(),
            "exhausting the parse retry budget must fail connect"
        );
    }

    #[tokio::test]
    async fn connect_queries_package_versions() {
        // Seed a tracked package before `connect()`, script its installed
        // version, and confirm `connect()` runs `query_versions()` (not just
        // `parse_system`) — mirrors
        // `query_versions_records_current_and_none_for_missing`.
        let conn =
            MockConnection::new("h1").with_default(CommandLog::new("", "bash 5.1-1\n", "", 0, 0));
        let mut t = enabled_with(conn);
        t.set_packages(vec![Package::new("bash")]);
        assert!(t.packages()[0].current().is_none());

        t.connect().await.expect("connect");

        assert!(t.packages()[0].current().is_some());
    }

    // --- connect-time lock check + stale-reap (target.py:187-188) -----------

    #[tokio::test]
    async fn connect_reaps_stale_foreign_lock() {
        // A foreign lock with a years-old timestamp is well past the default
        // 86400s stale age, so `connect()` force-removes it (upstream
        // `reap_if_stale`), and the host reads unlocked afterwards.
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.connect().await.expect("connect");

        assert!(
            handle
                .sftp_ops()
                .contains(&MockSftpOp::Remove(PathBuf::from(TARGET_LOCK_PATH))),
            "stale lock should have been force-removed"
        );
        assert!(
            !t.is_locked().await.expect("is_locked ok"),
            "host should read unlocked after reap"
        );
    }

    #[tokio::test]
    async fn connect_keeps_fresh_foreign_lock() {
        // A foreign lock younger than the stale age is *not* reaped — upstream
        // only warns. Timestamp is computed from the same clock the lock uses so
        // the "fresh" property holds regardless of wall-clock at test time.
        let recent = SystemClock.now_unix().saturating_sub(100);
        let line = format!("{recent}:alice:4242:busy");
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, line.into_bytes());
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.connect().await.expect("connect");

        assert!(
            !handle
                .sftp_ops()
                .contains(&MockSftpOp::Remove(PathBuf::from(TARGET_LOCK_PATH))),
            "a fresh lock must not be removed"
        );
        assert!(
            t.is_locked().await.expect("is_locked ok"),
            "fresh foreign lock should still be held"
        );
    }

    #[tokio::test]
    async fn connect_on_free_host_touches_no_lock() {
        // No lock file present: the check is a no-op (no reap), connect succeeds.
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.connect().await.expect("connect");

        assert!(
            !handle
                .sftp_ops()
                .iter()
                .any(|op| matches!(op, MockSftpOp::Remove(p) if p == Path::new(TARGET_LOCK_PATH))),
            "free host must not trigger a lock removal"
        );
    }

    #[tokio::test]
    async fn check_stale_lock_is_noop_when_unconnected() {
        // No connection ⇒ no lock object built ⇒ the check returns early.
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        t.check_stale_lock().await;
    }

    // --- reboot / reconnect / boot_id lifecycle (P2.9) ----------------------

    #[tokio::test]
    async fn boot_id_reads_proc_and_strips() {
        let conn = MockConnection::new("h1").with_response(
            "cat /proc/sys/kernel/random/boot_id",
            CommandLog::new(
                "cat /proc/sys/kernel/random/boot_id",
                "cab001af-4985-41d3-ac1b-e889523076ef\n",
                "",
                0,
                0,
            ),
        );
        let mut t = enabled_with(conn);
        assert_eq!(t.boot_id().await, "cab001af-4985-41d3-ac1b-e889523076ef");
    }

    #[tokio::test]
    async fn boot_id_returns_empty_on_failure() {
        // No scripted response + a timeout for the boot-id command -> read fails.
        let conn = MockConnection::new("h1").with_timeout("cat /proc/sys/kernel/random/boot_id");
        let mut t = enabled_with(conn);
        assert_eq!(t.boot_id().await, "");
    }

    #[tokio::test]
    async fn boot_id_empty_when_unconnected() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        assert_eq!(t.boot_id().await, "");
    }

    #[tokio::test]
    async fn reboot_fires_command_without_waiting() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.reboot("systemctl reboot").await;
        assert_eq!(handle.fired_commands(), vec!["systemctl reboot".to_owned()]);
    }

    #[tokio::test]
    async fn reboot_on_unconnected_is_noop() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        // Must not panic; nothing to assert beyond the no-op completing.
        t.reboot("systemctl reboot").await;
    }

    #[tokio::test]
    async fn close_without_action_just_closes() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.close(None).await.expect("close ok");
        assert!(
            handle.fired_commands().is_empty(),
            "no reboot/halt dispatched"
        );
        assert!(handle.is_closed(), "connection is closed");
    }

    #[tokio::test]
    async fn close_reboot_dispatches_reboot() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.close(Some("reboot")).await.expect("close ok");
        assert_eq!(handle.fired_commands(), vec!["reboot".to_owned()]);
    }

    #[tokio::test]
    async fn close_poweroff_dispatches_halt() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.close(Some("poweroff")).await.expect("close ok");
        assert_eq!(handle.fired_commands(), vec!["halt".to_owned()]);
    }

    #[tokio::test]
    async fn close_on_unconnected_is_noop() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        // Must not panic; no live connection to close, no action dispatched.
        t.close(Some("reboot")).await.expect("close ok");
    }

    #[tokio::test]
    async fn reconnect_delegates_to_connection() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.reconnect().await.expect("reconnect ok");
        assert_eq!(handle.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn reconnect_surfaces_failure() {
        let conn = MockConnection::new("h1").failing_reconnect();
        let mut t = enabled_with(conn);
        assert!(t.reconnect().await.is_err());
    }

    #[tokio::test]
    async fn reconnect_unconnected_is_ok_noop() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        t.reconnect().await.expect("noop reconnect ok");
    }

    // --- lock / is_locked delegators ----------------------------------------

    #[tokio::test]
    async fn is_locked_false_when_no_lock_file() {
        let conn = MockConnection::new("h1");
        let mut t = enabled_with(conn);
        assert!(!t.is_locked().await.expect("is_locked ok"));
    }

    #[tokio::test]
    async fn is_locked_true_when_foreign_lock_present() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let mut t = enabled_with(conn);
        assert!(t.is_locked().await.expect("is_locked ok"));
    }

    #[tokio::test]
    async fn is_locked_false_when_unconnected() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        assert!(!t.is_locked().await.expect("is_locked ok"));
    }

    #[tokio::test]
    async fn lock_writes_lockfile_on_free_host() {
        let conn = MockConnection::new("h1");
        let mut t = enabled_with(conn);
        t.lock("test comment").await.expect("lock ok");
        // Re-reading now reports locked (the lock file was created).
        assert!(t.is_locked().await.expect("is_locked ok"));
    }

    #[tokio::test]
    async fn lock_unconnected_is_ok_noop() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        t.lock("comment").await.expect("noop lock ok");
    }

    // --- lock_status (list_locks read side) ---------------------------------

    #[tokio::test]
    async fn lock_status_unlocked_on_free_host() {
        let mut t = enabled_with(MockConnection::new("h1"));
        let row = t.lock_status(false).await;
        assert!(!row.is_locked);
        assert_eq!(row, LockRow::default());
    }

    #[tokio::test]
    async fn lock_status_reports_foreign_operation_lock() {
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            b"1700000000:alice:4242:busy testing".to_vec(),
        );
        let mut t = enabled_with(conn);
        let row = t.lock_status(false).await;
        assert!(row.is_locked);
        assert!(!row.is_mine);
        assert_eq!(row.locked_by, "alice");
        assert_eq!(row.comment, "busy testing");
        assert!(row.time.ends_with("UTC"));
    }

    #[tokio::test]
    async fn lock_status_pool_reports_foreign_claim() {
        let conn = MockConnection::new("h1").with_file(
            POOL_LOCK_PATH,
            b"1700000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
        );
        let mut t = enabled_with(conn);
        let row = t.lock_status(true).await;
        assert!(row.is_locked);
        assert!(!row.is_mine);
        assert_eq!(row.locked_by, "bob");
        // The pool path fills the detail slot with the parsed RRID.
        assert_eq!(row.comment, "SUSE:Maintenance:9:9");
    }

    #[tokio::test]
    async fn lock_status_unconnected_is_unlocked() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        assert!(!t.lock_status(false).await.is_locked);
        assert!(!t.lock_status(true).await.is_locked);
    }

    /// Oracle for mtui-rs-0mop.4: resolving a full `LockRow` must read the
    /// lockfile exactly once, not once per derived field.
    #[tokio::test]
    async fn lock_status_reads_lockfile_once() {
        // Operation lock.
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            b"1700000000:alice:4242:busy testing".to_vec(),
        );
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        let _ = t.lock_status(false).await;
        let op_reads = handle
            .sftp_ops()
            .into_iter()
            .filter(|op| matches!(op, MockSftpOp::Open(p) if p == Path::new(TARGET_LOCK_PATH)))
            .count();
        assert_eq!(op_reads, 1, "operation lock should be read exactly once");

        // Pool claim.
        let conn = MockConnection::new("h1").with_file(
            POOL_LOCK_PATH,
            b"1700000000:bob:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec(),
        );
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        let _ = t.lock_status(true).await;
        let pool_reads = handle
            .sftp_ops()
            .into_iter()
            .filter(|op| matches!(op, MockSftpOp::Open(p) if p == Path::new(POOL_LOCK_PATH)))
            .count();
        assert_eq!(pool_reads, 1, "pool claim should be read exactly once");
    }

    /// A self-owned operation lock resolves `is_mine = true` off the single read.
    #[tokio::test]
    async fn lock_status_reports_self_owned_lock() {
        let me = mtui_config::Config::default().session_user;
        let pid = std::process::id();
        let line = format!("1700000000:{me}:{pid}:mine");
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, line.into_bytes());
        let mut t = enabled_with(conn);
        let row = t.lock_status(false).await;
        assert!(row.is_locked);
        assert!(row.is_mine);
        assert_eq!(row.locked_by, me);
        assert_eq!(row.comment, "mine");
    }

    // --- shell() (feature `shell`) ------------------------------------------

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_enabled_spawns_and_bridges() {
        let conn = MockConnection::new("h1")
            .with_shell_output(b"welcome\n".to_vec())
            .with_shell_output(b"$ ".to_vec());
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        let mut ch = t.shell(120, 40).await.expect("enabled spawns a shell");

        // Spawn recorded the requested PTY size.
        assert_eq!(handle.shell_spawns(), vec![(120, 40)]);

        // Output chunks drain in order, then EOF (0) — the bridge stop signal.
        let mut buf = [0u8; 64];
        let n = ch.read(&mut buf).await.expect("read 1");
        assert_eq!(&buf[..n], b"welcome\n");
        let n = ch.read(&mut buf).await.expect("read 2");
        assert_eq!(&buf[..n], b"$ ");
        let n = ch.read(&mut buf).await.expect("read eof");
        assert_eq!(n, 0, "channel EOF terminates the bridge loop");

        // Keystrokes and resizes are recorded through the shared mock.
        ch.write(b"ls\n").await.expect("write");
        ch.resize(80, 24).await.expect("resize");
        ch.close().await.expect("close");
        assert_eq!(handle.shell_input(), b"ls\n");
        assert_eq!(handle.shell_resizes(), vec![(80, 24)]);
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_dryrun_does_not_spawn() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Dryrun,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        assert!(t.shell(80, 24).await.is_none(), "dryrun must not spawn");
        assert!(handle.shell_spawns().is_empty());
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_disabled_does_not_spawn() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Disabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        assert!(t.shell(80, 24).await.is_none(), "disabled must not spawn");
        assert!(handle.shell_spawns().is_empty());
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_on_unconnected_target_returns_none() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        assert!(
            t.shell(80, 24).await.is_none(),
            "no connection -> no shell, logged not panicked"
        );
    }

    // --- packages / query_versions ----------------------------------------

    #[tokio::test]
    async fn query_versions_records_current_and_none_for_missing() {
        use mtui_types::package::Package;
        use mtui_types::rpmver::RPMVersion;

        let conn = MockConnection::new("h1").with_default(CommandLog::new(
            "",
            "bash 5.1-1\npackage foo is not installed\n",
            "",
            0,
            0,
        ));
        let mut t = enabled_with(conn);
        t.set_packages(vec![Package::new("bash"), Package::new("foo")]);
        t.query_versions().await;

        let by_name: std::collections::HashMap<_, _> =
            t.packages().iter().map(|p| (p.name.as_str(), p)).collect();
        assert_eq!(
            by_name["bash"].current(),
            Some(&RPMVersion::parse("5.1-1").unwrap())
        );
        assert_eq!(by_name["foo"].current(), None);
    }

    #[tokio::test]
    async fn query_versions_is_noop_without_tracked_packages() {
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "x", "", 0, 0));
        let mut t = enabled_with(conn);
        t.query_versions().await;
        // Nothing was queried, so the host log stays empty.
        assert!(t.out().is_empty());
    }

    // --- pool claim ---------------------------------------------------------

    #[tokio::test]
    async fn pool_unlock_noop_when_not_connected() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        // No pool lock built yet (unconnected): must not panic or fail.
        t.pool_unlock(false).await;
    }

    #[tokio::test]
    async fn with_connection_builds_a_live_pool_lock() {
        let conn = MockConnection::new("h1")
            .with_file(crate::POOL_LOCK_PATH, b"1700000000:testuser:99".to_vec());
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        // A `with_connection` target has a live pool lock; force-unlock removes
        // the pool file (proving the pool lock drives SFTP over the pool path).
        t.pool_unlock(true).await;
        assert!(handle.file_contents(crate::POOL_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn pool_unlock_swallows_foreign_claim() {
        let foreign = b"1700000000:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec();
        let conn = MockConnection::new("h1").with_file(crate::POOL_LOCK_PATH, foreign);
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.set_rrid("SUSE:Maintenance:1:2");
        // Best-effort: a foreign claim without force is swallowed (no panic), and
        // left in place.
        t.pool_unlock(false).await;
        assert!(handle.file_contents(crate::POOL_LOCK_PATH).is_some());
    }

    #[test]
    fn set_rrid_records_ownership_identity() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        t.set_rrid("SUSE:Maintenance:1:2");
        assert_eq!(t.rrid(), "SUSE:Maintenance:1:2");
    }

    #[tokio::test]
    async fn pool_claim_noop_false_when_not_connected() {
        let mut t = Target::new(&cfg(), "h1", TargetState::Enabled, ExecutionMode::Parallel);
        // No pool lock built yet (unconnected): claim returns false, no panic.
        assert!(!t.pool_claim("mtui pool RRID [RRID]").await.unwrap());
    }

    #[tokio::test]
    async fn pool_claim_succeeds_on_free_host() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);
        t.set_rrid("SUSE:Maintenance:1:2");
        assert!(
            t.pool_claim("mtui pool SUSE:Maintenance:1:2 [SUSE:Maintenance:1:2]")
                .await
                .unwrap(),
            "a free host must be claimable"
        );
        // The remote pool lock file was written.
        assert!(handle.file_contents(crate::POOL_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn pool_claim_false_on_foreign_claim() {
        let foreign = b"1700000000:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec();
        let conn = MockConnection::new("h1").with_file(crate::POOL_LOCK_PATH, foreign);
        let mut t = enabled_with(conn);
        t.set_rrid("SUSE:Maintenance:1:2");
        // Another process holds the remote claim → we lose the race.
        assert!(
            !t.pool_claim("mtui pool SUSE:Maintenance:1:2 [SUSE:Maintenance:1:2]")
                .await
                .unwrap(),
            "a host claimed by another process must not be claimable"
        );
    }

    // --- add_history -------------------------------------------------------

    #[tokio::test]
    async fn add_history_writes_timestamp_user_and_colon_joined_fields() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.add_history(&[
            "downgrade".to_owned(),
            "SUSE:Maintenance:1:2".to_owned(),
            "pkg-a pkg-b".to_owned(),
        ])
        .await;

        let written = String::from_utf8(
            handle
                .file_contents("/var/log/mtui.log")
                .expect("history file written"),
        )
        .unwrap();
        // Exactly one trailing-newline line.
        assert!(written.ends_with('\n'));
        let line = written.trim_end_matches('\n');
        // `timestamp:user:downgrade:SUSE:Maintenance:1:2:pkg-a pkg-b`.
        let mut it = line.splitn(3, ':');
        let ts = it.next().unwrap();
        assert!(
            ts.parse::<u64>().is_ok(),
            "leading field is a unix ts: {ts}"
        );
        let _user = it.next().unwrap();
        let rest = it.next().unwrap();
        assert_eq!(rest, "downgrade:SUSE:Maintenance:1:2:pkg-a pkg-b");
        // One append, no read-modify-write: the entry is sent via a single
        // `sftp_append`, never a read (`sftp_open`) + rewrite (`sftp_write`).
        assert_eq!(
            handle.sftp_ops(),
            vec![MockSftpOp::Append(PathBuf::from("/var/log/mtui.log"))]
        );
    }

    #[tokio::test]
    async fn add_history_appends_to_existing_contents() {
        let conn =
            MockConnection::new("h1").with_file("/var/log/mtui.log", b"prior:line\n".to_vec());
        let handle = conn.clone();
        let mut t = enabled_with(conn);

        t.add_history(&["install".to_owned(), "pkg".to_owned()])
            .await;

        let written =
            String::from_utf8(handle.file_contents("/var/log/mtui.log").unwrap()).unwrap();
        assert!(
            written.starts_with("prior:line\n"),
            "append preserves existing content: {written:?}"
        );
        assert_eq!(written.lines().count(), 2, "one line appended: {written:?}");
    }

    #[tokio::test]
    async fn add_history_skips_disabled_hosts() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut t = Target::with_connection(
            "h1",
            TargetState::Disabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );

        t.add_history(&["downgrade".to_owned(), "x".to_owned()])
            .await;

        assert!(
            handle.file_contents("/var/log/mtui.log").is_none(),
            "disabled hosts write no history"
        );
    }
}
