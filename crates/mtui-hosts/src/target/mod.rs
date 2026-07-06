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
//! * and a connection-only [`Target::connect`] that establishes the transport.
//!
//! The remaining upstream responsibilities are owned by later tasks and are
//! left as clearly-marked seams in [`Target::connect`]:
//!
//! * remote locks — the [`locks`] module (**P2.6**, landed) provides the
//!   zypper op-lock ([`TargetLock`]) and the pool-claim lock ([`PoolLock`]);
//!   wiring the lock *check* into `connect` needs session RRID plumbing from a
//!   later phase, so the connect-time hook stays a seam for now,
//! * system/product parsing (`parse_system`) and package querying — **P2.8**,
//! * reboot / reconnect lifecycle — **P2.9**; the install/uninstall
//!   [`Operation`](operation::Operation) template (skeleton + trait) has landed
//!   in [`operation`], driving its group via the object-safe
//!   [`OperationGroup`](operation::OperationGroup) seam. The
//!   `impl OperationGroup for HostsGroup` binding (which needs the Phase-4
//!   doer/check registries and the reboot wiring) is deferred to the
//!   composition root — see the `TODO` in [`operation`].
//!
//! Keeping the seams out of P2.4 preserves the acyclic crate graph
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

pub use actions::{Command, RunCommand, run_parallel, sftp_get_all, sftp_put_all, sftp_remove_all};
pub use arbiter::{HostArbiter, Owner, get_arbiter};
pub use hostgroup::HostsGroup;
pub use locks::{
    Clock, Lockable, POOL_LOCK_PATH, PoolLock, RemoteLock, SystemClock, TARGET_LOCK_PATH,
    TargetLock, with_locked,
};
pub use operation::{
    Check, CheckArgs, Doer, HostPlan, InstallOperation, LastOutput, Operation, OperationGroup,
    PlanProvider, UninstallOperation,
};
pub use package_querier::PackageQuerier;
pub use parsers::{parse_os_release, parse_product, parse_system};
pub use repo_manager::{RepoManager, RepoOp, SetRepo};
pub use reporter::Reporter;

use std::path::{Path, PathBuf};

use mtui_config::Config;
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::hostlog::{CommandLog, HostLog};
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

    /// Establishes the SSH transport for this target.
    ///
    /// This is the **connection-only** port of upstream `Target.connect`: it
    /// builds an [`SshConnection`] from the host/port/timeout/policy resolved at
    /// construction and stores it. If a connection is already attached (e.g. a
    /// test-injected [`MockConnection`](crate::MockConnection)), it is a no-op.
    ///
    /// Upstream's `connect` also, in order, checks the remote lock, parses the
    /// system/product, and queries installed package versions. Those steps are
    /// owned by later Phase 2 tasks and are intentionally **not** performed
    /// here — see the module docs. When they land they hook in right after the
    /// transport is established.
    ///
    /// # Errors
    ///
    /// Propagates [`HostError::Connect`] / [`HostError::Auth`] from
    /// [`SshConnection::connect`] when the host is unreachable or auth fails.
    pub async fn connect(&mut self) -> Result<()> {
        if self.connection.is_some() {
            tracing::debug!(host = %self.hostname, "already connected");
            return Ok(());
        }
        tracing::info!(host = %self.hostname, "connecting");
        let port = self.port.parse::<u16>().unwrap_or(0);
        let conn = SshConnection::connect(self.host.clone(), port, self.policy, self.timeout)
            .await
            .inspect_err(|e| {
                tracing::error!(host = %self.hostname, error = %e, "connecting to target failed");
            })?;
        self.connection = Some(Box::new(conn));

        // Deferred seams (do not implement here — they pull crates/behaviour
        // that P2.4 must not depend on):
        //   * remote lock check + stale-reap        -> P2.6 (locks)
        //   * parse_system / package version query  -> P2.8 (parsers)
        //   * reboot / reconnect / operation         -> P2.9 (operation)
        tracing::debug!(
            host = %self.hostname,
            "connected; lock/parse/query seams deferred to P2.6/P2.8/P2.9"
        );
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
        // A pre-injected (mock) connection means connect() short-circuits and
        // never touches the network.
        let mut t = enabled_with(MockConnection::new("test-host.example.com"));
        assert!(t.is_connected());
        t.connect().await.expect("noop connect");
        assert!(t.is_connected());
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
}
