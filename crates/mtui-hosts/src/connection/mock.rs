//! A scriptable [`Connection`] test double.
//!
//! Per the workspace testing conventions, host access is mocked rather than
//! hitting real sshd: unit tests drive a [`MockConnection`] entirely offline.
//! It records every command issued (so callers can assert ordering / fan-out),
//! serves canned [`CommandLog`] responses keyed by command (with a default),
//! and can be scripted to fail a specific command so the retry / timeout paths
//! in later Phase 2 tasks (P2.3 reconnect, P2.5 parallel fan-out) are testable.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mtui_types::hostlog::CommandLog;

use super::Connection;
#[cfg(feature = "shell")]
use super::ShellChannel;
use super::sftp_session::SftpSession;
use crate::error::{HostError, Result};

/// The outcome scripted for a command run against a [`MockConnection`].
#[derive(Debug, Clone)]
enum Outcome {
    /// Return this command log.
    Ok(CommandLog),
    /// Fail the run with a timeout for the command.
    #[cfg(test)]
    Timeout,
}

/// An SFTP operation observed by a [`MockConnection`], recorded in order so
/// tests can assert exactly what a caller did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockSftpOp {
    /// `sftp_put(local, remote)`.
    Put {
        /// The local source path.
        local: PathBuf,
        /// The remote destination path.
        remote: PathBuf,
    },
    /// `sftp_put_bytes(data, remote)` — records the payload length (not the
    /// bytes) and the destination, so a fan-out read-once test can assert that
    /// N hosts each received the same-sized shared payload.
    PutBytes {
        /// The number of bytes dispatched.
        len: usize,
        /// The remote destination path.
        remote: PathBuf,
    },
    /// `sftp_get(remote, local)`.
    Get {
        /// The remote source path.
        remote: PathBuf,
        /// The local destination path.
        local: PathBuf,
    },
    /// `sftp_get_folder(remote, local)`.
    GetFolder {
        /// The remote source folder.
        remote: PathBuf,
        /// The local destination folder.
        local: PathBuf,
    },
    /// `sftp_listdir(path)`.
    Listdir(PathBuf),
    /// `sftp_open(path)`.
    Open(PathBuf),
    /// `sftp_write(path, .., exclusive)`.
    Write {
        /// The remote path written.
        path: PathBuf,
        /// Whether the write was an exclusive (atomic-create) write.
        exclusive: bool,
    },
    /// `sftp_append(path, ..)`.
    Append(PathBuf),
    /// `sftp_remove(path)`.
    Remove(PathBuf),
    /// `sftp_rmdir(path)`.
    Rmdir(PathBuf),
    /// `sftp_readlink(path)`.
    Readlink(PathBuf),
}

/// A scriptable, in-memory [`Connection`] implementation for tests.
///
/// Construct with [`MockConnection::new`], script responses with
/// [`with_response`](MockConnection::with_response) /
/// [`with_default`](MockConnection::with_default) /
/// [`with_timeout`](MockConnection::with_timeout), then inspect issued commands
/// via [`commands`](MockConnection::commands).
#[derive(Debug, Clone)]
pub struct MockConnection {
    hostname: String,
    /// Per-command scripted outcomes.
    responses: HashMap<String, Outcome>,
    /// Fallback outcome when a command has no scripted response.
    default: Outcome,
    /// Artificial per-`run` delay so a test can model a long-running command
    /// (e.g. a multi-second `zypper` update) and observe the fan-out TTY spinner
    /// paint across the await. Zero (the default) is an instant response.
    run_delay: std::time::Duration,
    /// Whether the transport reports as active.
    active: bool,
    /// Commands issued, in order (shared so `Clone`d handles observe the same
    /// log — a `Box<dyn Connection>` may be moved but tests keep a handle).
    issued: Arc<Mutex<Vec<String>>>,
    /// Set once [`close`](Connection::close) has been called.
    closed: Arc<Mutex<bool>>,
    /// When set, [`close`](Connection::close) blocks until the [`Notify`] is
    /// fired before completing — models a wedged paramiko teardown (a dead peer
    /// with no RST) so a caller's bounded close budget can be exercised. Shared
    /// across `Clone`d handles so the test fires the same notify. Never set by
    /// default (an instant close).
    block_close: Option<Arc<tokio::sync::Notify>>,
    /// When set, [`close`](Connection::close) sleeps this long before completing,
    /// modelling a slow (but eventually-returning) host teardown. Unlike
    /// [`block_close`](Self::block_close) it needs no external release, so a
    /// fixed per-close cost can be timed. Never set by default (an instant close).
    close_delay: Option<std::time::Duration>,
    /// When `true`, [`close`](Connection::close) returns an error, modelling a
    /// host that fails to disconnect cleanly so `quit`'s per-host failure
    /// naming can be exercised.
    close_fails: bool,
    /// Number of times [`reconnect`](Connection::reconnect) has been called.
    reconnects: Arc<Mutex<usize>>,
    /// When `true`, [`reconnect`](Connection::reconnect) fails.
    reconnect_fails: bool,
    /// The `(retry, backoff)` args of the most recent
    /// [`reconnect`](Connection::reconnect) call, so a test can assert the
    /// caller passed the expected budget (e.g. the reboot lifecycle's
    /// `(config.reboot_retries, true)` vs. a fast-path `(0, false)`).
    last_reconnect_args: Arc<Mutex<Option<(usize, bool)>>>,
    /// Commands dispatched via [`fire_and_forget`](Connection::fire_and_forget).
    fired: Arc<Mutex<Vec<String>>>,
    /// SFTP operations observed, in order.
    sftp_ops: Arc<Mutex<Vec<MockSftpOp>>>,
    /// Number of batched SFTP sessions opened via
    /// [`sftp_session`](Connection::sftp_session). Shared across `Clone`d
    /// handles so a test observes every session regardless of which handle
    /// opened it — this is the `mtui-rs-0mop.3` handshake-count oracle
    /// (`parse_system` must open exactly one).
    sftp_sessions: Arc<Mutex<usize>>,
    /// Artificial delay charged **once per SFTP session open** (the
    /// channel+subsystem handshake). Models a high-latency host so a bench can
    /// show batching (one handshake) beat per-op (one handshake per read). Zero
    /// (the default) opens instantly.
    sftp_session_delay: std::time::Duration,
    /// Canned directory listings keyed by remote path (for `sftp_listdir` /
    /// `sftp_get_folder`).
    listings: HashMap<PathBuf, Vec<String>>,
    /// File contents keyed by remote path (for `sftp_open` / `sftp_write`).
    ///
    /// Shared + mutable so `sftp_write` can create/overwrite entries and a
    /// later `sftp_open` observes them — this is what makes the lock protocol
    /// (exclusive create, reconcile, read-back) testable end-to-end against the
    /// mock. `Clone`d handles share the same table.
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    /// Canned symlink targets keyed by remote path (for `sftp_readlink`).
    links: HashMap<PathBuf, String>,
    /// When `true`, [`sftp_remove`](Connection::sftp_remove) fails with a generic
    /// [`HostError::Sftp`], exercising a caller's directory-removal fallback
    /// (e.g. `Target::sftp_remove`) or the fail-closed unlock path (a non-gone
    /// removal error must propagate).
    sftp_remove_fails: bool,
    /// When `true`, [`sftp_remove`](Connection::sftp_remove) fails with
    /// [`HostError::SftpNotFound`] (the file is already gone), so the unlock
    /// "ignore only already-missing" path can be exercised.
    sftp_remove_not_found: bool,
    /// When `true`, [`sftp_get`](Connection::sftp_get) /
    /// [`sftp_get_folder`](Connection::sftp_get_folder) fail with a generic
    /// [`HostError::Sftp`], so a caller's per-host download outcome tracking can
    /// be exercised. The op is still recorded before failing.
    sftp_get_fails: bool,
    /// When `Some`, the boot-id probe (`cat /proc/sys/kernel/random/boot_id`)
    /// returns a *fresh* value on every call (`boot-<n>`, incrementing across
    /// `Clone`d handles), modelling a host that actually rebooted (the pre- and
    /// post-reboot reads differ). `None` (the default) leaves the boot-id command
    /// answered by the normal response/default map, so scripting the same fixed
    /// id both times models a host that did *not* reboot.
    boot_id_counter: Option<Arc<Mutex<u64>>>,
    /// File paths whose *exclusive* [`sftp_write`](Connection::sftp_write) fails
    /// with a generic [`HostError::Sftp`] instead of the contention
    /// [`HostError::AlreadyExists`] — models a non-collision failure of the
    /// atomic create (permission denied, transport), which must fail closed
    /// rather than reconcile.
    exclusive_write_errors: HashSet<PathBuf>,
    /// Paths scripted to raise [`HostError::Sftp`] from
    /// [`sftp_append`](Connection::sftp_append), modelling a best-effort append
    /// failure (read-only/full remote fs) that callers such as `add_history`
    /// swallow.
    sftp_append_errors: HashSet<PathBuf>,
    /// When `Some`, [`sftp_put`](Connection::sftp_put) /
    /// [`sftp_put_bytes`](Connection::sftp_put_bytes) fail with
    /// [`HostError::Sftp`] carrying this reason, so a caller's per-host upload
    /// outcome tracking can be exercised. `None` (the default) keeps both `Ok`.
    fail_sftp_put: Option<String>,
    /// Directory paths scripted to raise [`HostError::SftpNotFound`] from
    /// `sftp_listdir` (mirrors upstream `listdir` raising `OSError`, e.g. a host
    /// with no `/etc/products.d`). Distinct from unscripted paths, which return
    /// an empty listing.
    missing_dirs: HashSet<PathBuf>,
    /// File paths scripted to raise a generic [`HostError::Sftp`] (non
    /// not-found) from `sftp_open`, mirroring a dangling symlink whose target
    /// product file open raises `OSError` rather than `FileNotFoundError`.
    sftp_open_errors: HashSet<PathBuf>,
    /// Directory paths scripted to raise a *transient* [`HostError::Sftp`] on
    /// the first N `sftp_listdir` calls, then succeed — models a flaky SFTP
    /// session (e.g. a connect-time timeout) that a bounded retry recovers from.
    /// The count is shared+mutable so it decrements across `Clone`d handles.
    listdir_transient_failures: Arc<Mutex<HashMap<PathBuf, u32>>>,
    /// Canned bytes served by [`ShellChannel::read`] on a spawned shell, drained
    /// one chunk per `read` then `0` (EOF). Lets the Phase-6 TTY bridge be
    /// tested offline.
    #[cfg(feature = "shell")]
    shell_output: Vec<Vec<u8>>,
    /// PTY sizes requested via [`shell`](Connection::shell), in order, so a
    /// caller can assert the spawn dimensions.
    #[cfg(feature = "shell")]
    shell_spawns: Arc<Mutex<Vec<(u32, u32)>>>,
    /// Bytes written to spawned shells via [`ShellChannel::write`], concatenated
    /// across all channels, so a caller can assert the keystrokes sent.
    #[cfg(feature = "shell")]
    shell_input: Arc<Mutex<Vec<u8>>>,
    /// Resize requests observed via [`ShellChannel::resize`], in order.
    #[cfg(feature = "shell")]
    shell_resizes: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl MockConnection {
    /// Creates a mock for `hostname` whose default response is an empty,
    /// successful [`CommandLog`] (exit code 0).
    #[must_use]
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            responses: HashMap::new(),
            default: Outcome::Ok(CommandLog::new("", "", "", 0, 0)),
            run_delay: std::time::Duration::ZERO,
            active: true,
            issued: Arc::new(Mutex::new(Vec::new())),
            closed: Arc::new(Mutex::new(false)),
            block_close: None,
            close_delay: None,
            close_fails: false,
            reconnects: Arc::new(Mutex::new(0)),
            reconnect_fails: false,
            last_reconnect_args: Arc::new(Mutex::new(None)),
            fired: Arc::new(Mutex::new(Vec::new())),
            sftp_ops: Arc::new(Mutex::new(Vec::new())),
            sftp_sessions: Arc::new(Mutex::new(0)),
            sftp_session_delay: std::time::Duration::ZERO,
            listings: HashMap::new(),
            files: Arc::new(Mutex::new(HashMap::new())),
            links: HashMap::new(),
            sftp_remove_fails: false,
            sftp_remove_not_found: false,
            sftp_get_fails: false,
            boot_id_counter: None,
            exclusive_write_errors: HashSet::new(),
            sftp_append_errors: HashSet::new(),
            fail_sftp_put: None,
            missing_dirs: HashSet::new(),
            sftp_open_errors: HashSet::new(),
            listdir_transient_failures: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "shell")]
            shell_output: Vec::new(),
            #[cfg(feature = "shell")]
            shell_spawns: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "shell")]
            shell_input: Arc::new(Mutex::new(Vec::new())),
            #[cfg(feature = "shell")]
            shell_resizes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Makes [`sftp_remove`](Connection::sftp_remove) fail with a generic
    /// [`HostError::Sftp`] so a caller's directory-removal fallback path, or the
    /// fail-closed unlock path (a non-gone removal error must propagate), can be
    /// exercised.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn failing_sftp_remove(mut self) -> Self {
        self.sftp_remove_fails = true;
        self
    }

    /// Makes [`sftp_remove`](Connection::sftp_remove) fail with
    /// [`HostError::SftpNotFound`] (the file is already gone), so the unlock
    /// "ignore only already-missing" path can be exercised.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn not_found_sftp_remove(mut self) -> Self {
        self.sftp_remove_not_found = true;
        self
    }

    /// Makes [`sftp_get`](Connection::sftp_get) /
    /// [`sftp_get_folder`](Connection::sftp_get_folder) fail with a generic
    /// [`HostError::Sftp`], so a caller's per-host download outcome tracking
    /// ([`Target::sftp_get`]) can be exercised. The op is still recorded before
    /// failing.
    #[must_use]
    pub fn failing_sftp_get(mut self) -> Self {
        self.sftp_get_fails = true;
        self
    }

    /// Makes the boot-id probe return a *fresh* value on every call, modelling a
    /// host that actually rebooted (the pre- and post-reboot reads differ) so the
    /// group reboot's boot-id verification records success. Without it, scripting
    /// a fixed boot id both times models a host that did *not* reboot (unchanged
    /// id ⇒ recorded failure).
    #[must_use]
    pub fn with_changing_boot_id(mut self) -> Self {
        self.boot_id_counter = Some(Arc::new(Mutex::new(0)));
        self
    }

    /// Scripts an *exclusive* [`sftp_write`](Connection::sftp_write) to `path` to
    /// fail with a generic [`HostError::Sftp`] — a non-collision failure of the
    /// atomic create (e.g. permission denied). Unlike a real collision (which
    /// returns [`HostError::AlreadyExists`]), this must fail closed and
    /// propagate, not reconcile.
    #[must_use]
    pub fn with_exclusive_write_error(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclusive_write_errors.insert(path.into());
        self
    }

    /// Scripts `path`'s [`sftp_append`](Connection::sftp_append) calls to raise a
    /// [`HostError::Sftp`], modelling a best-effort append failure (read-only or
    /// full remote fs) that a caller such as `add_history` swallows.
    #[must_use]
    pub fn with_sftp_append_error(mut self, path: impl Into<PathBuf>) -> Self {
        self.sftp_append_errors.insert(path.into());
        self
    }

    /// Scripts [`sftp_put`](Connection::sftp_put) /
    /// [`sftp_put_bytes`](Connection::sftp_put_bytes) to fail with a
    /// [`HostError::Sftp`] carrying `msg`, so a caller's per-host upload outcome
    /// tracking can be exercised. The op is still recorded before failing.
    #[must_use]
    pub fn with_sftp_put_failure(mut self, msg: impl Into<String>) -> Self {
        self.fail_sftp_put = Some(msg.into());
        self
    }

    /// Scripts `path`'s first `count` [`sftp_listdir`](Connection::sftp_listdir)
    /// calls to raise a transient [`HostError::Sftp`] before succeeding, so a
    /// caller's bounded retry (e.g. `Target::connect`'s system-parse retry) can
    /// be exercised against a flaky session.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_transient_listdir_failures(
        self,
        path: impl Into<PathBuf>,
        count: u32,
    ) -> Self {
        self.listdir_transient_failures
            .lock()
            .expect("mock transient-failures lock")
            .insert(path.into(), count);
        self
    }

    /// Scripts a full [`CommandLog`] response for an exact command string.
    #[must_use]
    pub fn with_response(mut self, command: impl Into<String>, log: CommandLog) -> Self {
        self.responses.insert(command.into(), Outcome::Ok(log));
        self
    }

    /// Scripts `command` to time out (surfaced as [`HostError::Timeout`]).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_timeout(mut self, command: impl Into<String>) -> Self {
        self.responses.insert(command.into(), Outcome::Timeout);
        self
    }

    /// Overrides the fallback response used when a command is not explicitly
    /// scripted.
    #[must_use]
    pub fn with_default(mut self, log: CommandLog) -> Self {
        self.default = Outcome::Ok(log);
        self
    }

    /// Adds an artificial delay to every [`run`](Connection::run) so a test can
    /// model a long-running command and observe the fan-out TTY spinner painting
    /// across the await.
    #[must_use]
    pub fn with_run_delay(mut self, delay: std::time::Duration) -> Self {
        self.run_delay = delay;
        self
    }

    /// Adds an artificial delay charged **once per SFTP session open**, modelling
    /// a high-latency host's channel+subsystem handshake so a bench can contrast
    /// batching (one handshake for a whole probe) against per-op reads (one
    /// handshake each).
    #[must_use]
    pub fn with_sftp_session_delay(mut self, delay: std::time::Duration) -> Self {
        self.sftp_session_delay = delay;
        self
    }

    /// Marks the transport as inactive (e.g. to test `is_active` handling).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn inactive(mut self) -> Self {
        self.active = false;
        self
    }

    /// Makes [`close`](Connection::close) block until `gate` is notified, modelling
    /// a wedged host teardown (a dead peer whose close never returns). A caller's
    /// bounded close budget (e.g. `McpSession::close`) can then be shown to still
    /// return; the test fires `gate` afterwards so the abandoned close unwinds.
    #[must_use]
    pub fn with_blocking_close(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.block_close = Some(gate);
        self
    }

    /// Makes [`close`](Connection::close) sleep `delay` before completing,
    /// modelling a slow-but-eventually-returning host teardown. Unlike
    /// [`with_blocking_close`](Self::with_blocking_close) it self-releases, so a
    /// fixed per-close cost can be timed (e.g. proving the idle sweeper tears
    /// stale sessions down concurrently rather than serially).
    #[must_use]
    pub fn with_close_delay(mut self, delay: std::time::Duration) -> Self {
        self.close_delay = Some(delay);
        self
    }

    /// Scripts [`close`](Connection::close) to fail, modelling a host that does
    /// not disconnect cleanly so `quit`'s per-host failure naming (upstream
    /// `failed to disconnect from <host>`) can be exercised.
    #[must_use]
    pub fn with_failing_close(mut self) -> Self {
        self.close_fails = true;
        self
    }

    /// Scripts [`reconnect`](Connection::reconnect) to fail with
    /// [`HostError::ReconnectFailed`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn failing_reconnect(mut self) -> Self {
        self.reconnect_fails = true;
        self
    }

    /// Scripts a canned directory listing for `sftp_listdir` /
    /// `sftp_get_folder` on `path`.
    #[must_use]
    pub fn with_listing(
        mut self,
        path: impl Into<PathBuf>,
        entries: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.listings
            .insert(path.into(), entries.into_iter().map(Into::into).collect());
        self
    }

    /// Scripts canned file contents for `sftp_open` on `path`.
    #[must_use]
    pub fn with_file(self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        self.files
            .lock()
            .expect("mock files lock")
            .insert(path.into(), contents.into());
        self
    }

    /// Returns the current in-memory contents of a remote file written via
    /// [`sftp_write`](Connection::sftp_write) (or seeded with
    /// [`with_file`](Self::with_file)), or `None` when absent.
    #[must_use]
    pub fn file_contents(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        self.files
            .lock()
            .expect("mock files lock")
            .get(path.as_ref())
            .cloned()
    }

    /// Returns every path currently present in the in-memory file table (seeded
    /// via [`with_file`](Self::with_file) or written by an SFTP operation), so a
    /// test can assert on the *complete set* of writes — e.g. that a folder
    /// download produced no key outside its intended destination.
    #[must_use]
    pub fn file_paths(&self) -> Vec<PathBuf> {
        self.files
            .lock()
            .expect("mock files lock")
            .keys()
            .cloned()
            .collect()
    }

    /// Scripts a canned symlink target for `sftp_readlink` on `path`.
    #[must_use]
    pub fn with_link(mut self, path: impl Into<PathBuf>, target: impl Into<String>) -> Self {
        self.links.insert(path.into(), target.into());
        self
    }

    /// Scripts a directory `path` to raise [`HostError::SftpNotFound`] from
    /// [`sftp_listdir`](Connection::sftp_listdir), mirroring a host whose
    /// directory does not exist (upstream `listdir` raising `OSError`). Without
    /// this, unscripted directories return an empty listing.
    #[must_use]
    pub fn with_missing_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.missing_dirs.insert(path.into());
        self
    }

    /// Scripts a file `path` to raise a generic (non not-found)
    /// [`HostError::Sftp`] from [`sftp_open`](Connection::sftp_open), mirroring a
    /// dangling symlink whose target product file open raises `OSError` rather
    /// than `FileNotFoundError`.
    #[must_use]
    pub fn with_open_error(mut self, path: impl Into<PathBuf>) -> Self {
        self.sftp_open_errors.insert(path.into());
        self
    }

    /// Returns a snapshot of the commands issued so far, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<String> {
        self.issued.lock().expect("mock issued lock").clone()
    }

    /// Returns whether [`close`](Connection::close) has been called.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.closed.lock().expect("mock closed lock")
    }

    /// Returns how many times [`reconnect`](Connection::reconnect) was called.
    #[must_use]
    pub fn reconnect_count(&self) -> usize {
        *self.reconnects.lock().expect("mock reconnects lock")
    }

    /// The `(retry, backoff)` args of the most recent
    /// [`reconnect`](Connection::reconnect) call, or `None` if never called.
    #[cfg(test)]
    pub(crate) fn last_reconnect_args(&self) -> Option<(usize, bool)> {
        *self
            .last_reconnect_args
            .lock()
            .expect("mock last reconnect args lock")
    }

    /// Returns the commands dispatched via
    /// [`fire_and_forget`](Connection::fire_and_forget), in order.
    #[must_use]
    pub fn fired_commands(&self) -> Vec<String> {
        self.fired.lock().expect("mock fired lock").clone()
    }

    /// Returns the SFTP operations observed so far, in order.
    #[must_use]
    pub fn sftp_ops(&self) -> Vec<MockSftpOp> {
        self.sftp_ops.lock().expect("mock sftp lock").clone()
    }

    /// Returns how many batched SFTP sessions were opened via
    /// [`sftp_session`](Connection::sftp_session).
    ///
    /// The `mtui-rs-0mop.3` handshake-count oracle: a multi-read probe
    /// (`parse_system`) that batches correctly opens exactly **one** session
    /// regardless of how many files it reads.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn sftp_session_count(&self) -> usize {
        *self.sftp_sessions.lock().expect("mock sftp sessions lock")
    }

    fn record_sftp(&self, op: MockSftpOp) {
        self.sftp_ops.lock().expect("mock sftp lock").push(op);
    }

    /// The file-lookup body of `sftp_open` **without** the per-op handshake
    /// delay: records the op and returns the scripted bytes/error. Used by the
    /// batched [`SftpSession`] (which already paid the handshake once at session
    /// open) so a batch of reads pays one handshake, not one per read.
    fn open_no_handshake(&self, path: &Path) -> Result<Vec<u8>> {
        self.record_sftp(MockSftpOp::Open(path.to_path_buf()));
        if self.sftp_open_errors.contains(path) {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("open failed: {}", path.display()),
            });
        }
        self.files
            .lock()
            .expect("mock files lock")
            .get(path)
            .cloned()
            .ok_or_else(|| HostError::SftpNotFound {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            })
    }

    /// Scripts one chunk of shell output served by
    /// [`ShellChannel::read`](crate::connection::ShellChannel::read) on a
    /// spawned shell. Chunks are drained in order, one per `read`, then `read`
    /// returns `0` (EOF) — the bridge loop's stop condition.
    #[cfg(feature = "shell")]
    #[must_use]
    pub fn with_shell_output(mut self, chunk: impl Into<Vec<u8>>) -> Self {
        self.shell_output.push(chunk.into());
        self
    }

    /// Returns the PTY sizes requested via [`shell`](Connection::shell), in
    /// order (`(cols, rows)`).
    #[cfg(feature = "shell")]
    #[must_use]
    pub fn shell_spawns(&self) -> Vec<(u32, u32)> {
        self.shell_spawns
            .lock()
            .expect("mock shell spawns lock")
            .clone()
    }

    /// Returns the bytes written to spawned shells via
    /// [`ShellChannel::write`](crate::connection::ShellChannel::write),
    /// concatenated in order.
    #[cfg(all(feature = "shell", test))]
    #[must_use]
    pub(crate) fn shell_input(&self) -> Vec<u8> {
        self.shell_input
            .lock()
            .expect("mock shell input lock")
            .clone()
    }

    /// Returns the resize requests observed via
    /// [`ShellChannel::resize`](crate::connection::ShellChannel::resize), in
    /// order (`(cols, rows)`).
    #[cfg(all(feature = "shell", test))]
    #[must_use]
    pub(crate) fn shell_resizes(&self) -> Vec<(u32, u32)> {
        self.shell_resizes
            .lock()
            .expect("mock shell resizes lock")
            .clone()
    }
}

/// A scriptable in-memory [`ShellChannel`] returned by
/// [`MockConnection::shell`], so the Phase-6 TTY bridge is testable offline.
///
/// Mirrors the real [`SshShellChannel`](crate::connection::SshConnection)
/// read semantics: a scripted chunk larger than the caller's buffer is served
/// in pieces across successive `read`s (leftover carryover) rather than
/// truncated, so the mock stays a faithful double for the CLI bridge tests.
#[cfg(feature = "shell")]
struct MockShellChannel {
    /// Canned output chunks, drained front-to-back.
    output: std::collections::VecDeque<Vec<u8>>,
    /// Bytes of the current chunk not yet returned to a caller.
    leftover: Vec<u8>,
    /// Shared keystroke sink (the parent mock's `shell_input`).
    input: Arc<Mutex<Vec<u8>>>,
    /// Shared resize log (the parent mock's `shell_resizes`).
    resizes: Arc<Mutex<Vec<(u32, u32)>>>,
}

#[cfg(feature = "shell")]
impl MockShellChannel {
    fn serve(&mut self, data: &[u8], buf: &mut [u8]) -> usize {
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        if n < data.len() {
            self.leftover = data[n..].to_vec();
        }
        n
    }
}

#[cfg(feature = "shell")]
#[async_trait]
impl ShellChannel for MockShellChannel {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if !self.leftover.is_empty() {
            let carried = std::mem::take(&mut self.leftover);
            return Ok(self.serve(&carried, buf));
        }
        match self.output.pop_front() {
            Some(chunk) => Ok(self.serve(&chunk, buf)),
            None => Ok(0),
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.input
            .lock()
            .expect("mock shell input lock")
            .extend_from_slice(data);
        Ok(())
    }

    async fn resize(&mut self, cols: u32, rows: u32) -> Result<()> {
        self.resizes
            .lock()
            .expect("mock shell resizes lock")
            .push((cols, rows));
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A batched [`SftpSession`] over a [`MockConnection`], returned by
/// [`MockConnection::sftp_session`].
///
/// Holds a `Clone` of the parent mock (which shares its scripted state via
/// `Arc`) and delegates each read verb to the parent's per-op `sftp_*` method,
/// so the batched and per-op read paths honor identical scripting, error
/// injection, and op-recording — there is exactly one source of truth for mock
/// SFTP-read behaviour.
struct MockSftpSession {
    conn: MockConnection,
}

#[async_trait]
impl SftpSession for MockSftpSession {
    async fn open(&mut self, path: &Path) -> Result<Vec<u8>> {
        // No per-read handshake: the session already paid it once at open.
        self.conn.open_no_handshake(path)
    }

    async fn listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        self.conn.sftp_listdir(path).await
    }

    async fn readlink(&mut self, path: &Path) -> Result<String> {
        self.conn.sftp_readlink(path).await
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Connection for MockConnection {
    fn hostname(&self) -> &str {
        &self.hostname
    }

    fn clone_box(&self) -> Box<dyn Connection> {
        // `MockConnection` is `Clone` and shares its scripted state (issued
        // commands, files, sftp ops) via `Arc`, so the clone observes the same
        // log — a lock built from it force-unlocks against the same mock.
        Box::new(self.clone())
    }

    async fn run(&mut self, command: &str) -> Result<CommandLog> {
        self.issued
            .lock()
            .expect("mock issued lock")
            .push(command.to_owned());

        if !self.run_delay.is_zero() {
            tokio::time::sleep(self.run_delay).await;
        }

        // A changing boot-id models a host that actually rebooted: each probe
        // returns a fresh value, so the pre- and post-reboot reads differ.
        if command == "cat /proc/sys/kernel/random/boot_id"
            && let Some(counter) = &self.boot_id_counter
        {
            let mut n = counter.lock().expect("mock boot-id counter lock");
            *n += 1;
            return Ok(CommandLog::new(command, format!("boot-{n}\n"), "", 0, 0));
        }

        let outcome = self.responses.get(command).unwrap_or(&self.default);
        match outcome {
            Outcome::Ok(log) => Ok(log.clone()),
            #[cfg(test)]
            Outcome::Timeout => Err(HostError::Timeout {
                command: command.to_owned(),
            }),
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    async fn close(&mut self) -> Result<()> {
        // Model a wedged teardown: block until the test releases the gate. A
        // caller with a bounded close budget abandons the await before this
        // returns; `closed` is only set once (if) the gate fires.
        if let Some(gate) = self.block_close.clone() {
            gate.notified().await;
        }
        if let Some(delay) = self.close_delay {
            tokio::time::sleep(delay).await;
        }
        if self.close_fails {
            return Err(HostError::Connect {
                host: self.hostname.clone(),
                reason: "mock close failure".to_owned(),
            });
        }
        *self.closed.lock().expect("mock closed lock") = true;
        self.active = false;
        Ok(())
    }

    async fn reconnect(&mut self, retry: usize, backoff: bool) -> Result<()> {
        *self.reconnects.lock().expect("mock reconnects lock") += 1;
        *self
            .last_reconnect_args
            .lock()
            .expect("mock last reconnect args lock") = Some((retry, backoff));
        if self.reconnect_fails {
            return Err(HostError::ReconnectFailed {
                host: self.hostname.clone(),
            });
        }
        self.active = true;
        Ok(())
    }

    async fn fire_and_forget(&mut self, command: &str) -> Result<()> {
        self.fired
            .lock()
            .expect("mock fired lock")
            .push(command.to_owned());
        // Mirrors upstream: dispatch, then tear down the local link.
        self.active = false;
        *self.closed.lock().expect("mock closed lock") = true;
        Ok(())
    }

    async fn sftp_put(&mut self, local: &Path, remote: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Put {
            local: local.to_path_buf(),
            remote: remote.to_path_buf(),
        });
        if let Some(reason) = &self.fail_sftp_put {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: reason.clone(),
            });
        }
        Ok(())
    }

    async fn sftp_put_bytes(&mut self, data: &[u8], remote: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::PutBytes {
            len: data.len(),
            remote: remote.to_path_buf(),
        });
        if let Some(reason) = &self.fail_sftp_put {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: reason.clone(),
            });
        }
        Ok(())
    }

    async fn sftp_get(&mut self, remote: &Path, local: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Get {
            remote: remote.to_path_buf(),
            local: local.to_path_buf(),
        });
        if self.sftp_get_fails {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: "scripted sftp_get failure".to_owned(),
            });
        }
        Ok(())
    }

    async fn sftp_get_folder(&mut self, remote: &Path, local: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::GetFolder {
            remote: remote.to_path_buf(),
            local: local.to_path_buf(),
        });
        if self.sftp_get_fails {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: "scripted sftp_get_folder failure".to_owned(),
            });
        }
        // Mirror the real `SshConnection::sftp_get_folder` loop so the
        // path-traversal trust boundary is exercised end-to-end: iterate the
        // canned listing, validate each server-supplied name through the *same*
        // helper the ssh impl uses (single source of truth), skip rejects, and
        // for accepted names copy the remote file bytes into the local target
        // key (`<local><name>.<hostname>`). Accepted vs rejected is then
        // observable via `file_contents` / the `files` table.
        let remote_str = remote.to_string_lossy().to_string();
        let names = self.listings.get(remote).cloned().unwrap_or_default();
        for name in names {
            if super::ssh::validate_sftp_component(&name, &self.hostname).is_err() {
                continue;
            }
            let src = PathBuf::from(format!("{remote_str}/{name}"));
            let data = {
                let files = self.files.lock().expect("mock files lock");
                files.get(&src).cloned().unwrap_or_default()
            };
            let target = PathBuf::from(format!(
                "{}{}.{}",
                local.to_string_lossy(),
                name,
                self.hostname
            ));
            self.files
                .lock()
                .expect("mock files lock")
                .insert(target, data);
        }
        Ok(())
    }

    async fn sftp_listdir(&mut self, path: &Path) -> Result<Vec<String>> {
        self.record_sftp(MockSftpOp::Listdir(path.to_path_buf()));
        // Transient-failure script: fail the first N calls for this path, then
        // fall through to the normal listing so a retry recovers.
        {
            let mut pending = self
                .listdir_transient_failures
                .lock()
                .expect("mock transient-failures lock");
            if let Some(remaining) = pending.get_mut(path)
                && *remaining > 0
            {
                *remaining -= 1;
                return Err(HostError::Sftp {
                    host: self.hostname.clone(),
                    reason: "Timeout".to_owned(),
                });
            }
        }
        if self.missing_dirs.contains(path) {
            return Err(HostError::SftpNotFound {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            });
        }
        Ok(self.listings.get(path).cloned().unwrap_or_default())
    }

    async fn sftp_open(&mut self, path: &Path) -> Result<Vec<u8>> {
        // The per-op path opens its own SFTP session per call (real ssh:
        // `self.sftp()`), so charge the handshake delay here — this is the
        // once-per-read cost the 0mop.3 batched path amortizes to once-per-probe.
        if !self.sftp_session_delay.is_zero() {
            tokio::time::sleep(self.sftp_session_delay).await;
        }
        self.open_no_handshake(path)
    }

    async fn sftp_write(&mut self, path: &Path, data: &[u8], exclusive: bool) -> Result<()> {
        self.record_sftp(MockSftpOp::Write {
            path: path.to_path_buf(),
            exclusive,
        });
        if exclusive && self.exclusive_write_errors.contains(path) {
            // Non-collision failure of the atomic create (e.g. permission
            // denied): must fail closed and propagate, not reconcile.
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("scripted exclusive-create failure: {}", path.display()),
            });
        }
        let mut files = self.files.lock().expect("mock files lock");
        if exclusive && files.contains_key(path) {
            // Atomic exclusive create lost the race: the file already exists.
            return Err(HostError::AlreadyExists {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            });
        }
        files.insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    async fn sftp_append(&mut self, path: &Path, data: &[u8]) -> Result<()> {
        self.record_sftp(MockSftpOp::Append(path.to_path_buf()));
        if self.sftp_append_errors.contains(path) {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: format!("scripted append failure: {}", path.display()),
            });
        }
        // Additive: create-if-missing, then extend at EOF. No truncation and no
        // read-modify-write window, so concurrent appenders never lose entries.
        let mut files = self.files.lock().expect("mock files lock");
        files
            .entry(path.to_path_buf())
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }

    async fn sftp_remove(&mut self, path: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Remove(path.to_path_buf()));
        if self.sftp_remove_not_found {
            return Err(HostError::SftpNotFound {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            });
        }
        if self.sftp_remove_fails {
            return Err(HostError::Sftp {
                host: self.hostname.clone(),
                reason: "scripted sftp_remove failure".to_owned(),
            });
        }
        // Actually drop the file so a later `sftp_open` reflects the removal —
        // this makes the lock lifecycle (lock → unlock → is_locked) observable
        // end-to-end against the mock, not just via the recorded op log.
        self.files.lock().expect("mock files lock").remove(path);
        Ok(())
    }

    async fn sftp_rmdir(&mut self, path: &Path) -> Result<()> {
        self.record_sftp(MockSftpOp::Rmdir(path.to_path_buf()));
        Ok(())
    }

    async fn sftp_readlink(&mut self, path: &Path) -> Result<String> {
        self.record_sftp(MockSftpOp::Readlink(path.to_path_buf()));
        // An unscripted path models a missing symlink on a real host, which the
        // SFTP layer reports as `SftpNotFound` (not a generic error) — matching
        // `sftp_open`'s missing-file semantics. `parse_system` relies on this to
        // degrade a missing `baseproduct` to a dangling base rather than a hard
        // parse failure.
        self.links
            .get(path)
            .cloned()
            .ok_or_else(|| HostError::SftpNotFound {
                host: self.hostname.clone(),
                path: path.display().to_string(),
            })
    }

    async fn sftp_session(&mut self) -> Result<Box<dyn SftpSession + '_>> {
        // Model the per-batch handshake: count one session open. Reconnect at
        // entry if inactive, mirroring the ssh impl (and `parse_system`'s
        // reconnect-then-retry expectations under `Target::connect`).
        if !self.active {
            self.reconnect(0, false).await?;
        }
        if !self.sftp_session_delay.is_zero() {
            tokio::time::sleep(self.sftp_session_delay).await;
        }
        *self.sftp_sessions.lock().expect("mock sftp sessions lock") += 1;
        // The handle shares this mock's scripted state via `Arc` (clone), so its
        // reads record into the same `sftp_ops` log and honor the same
        // file/listing/link/error scripting as the per-op methods — no behaviour
        // divergence between the batched and per-op read paths.
        Ok(Box::new(MockSftpSession { conn: self.clone() }))
    }

    #[cfg(feature = "shell")]
    async fn shell(&mut self, cols: u32, rows: u32) -> Result<Box<dyn ShellChannel>> {
        self.shell_spawns
            .lock()
            .expect("mock shell spawns lock")
            .push((cols, rows));
        Ok(Box::new(MockShellChannel {
            output: self.shell_output.iter().cloned().collect(),
            leftover: Vec::new(),
            input: Arc::clone(&self.shell_input),
            resizes: Arc::clone(&self.shell_resizes),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_response_is_success() {
        let mut conn = MockConnection::new("h1");
        let log = conn.run("uptime").await.expect("run ok");
        assert_eq!(log.exitcode, 0);
        assert_eq!(conn.hostname(), "h1");
    }

    #[tokio::test]
    async fn scripted_response_is_returned() {
        let mut conn = MockConnection::new("h1").with_response(
            "cat /etc/os-release",
            CommandLog::new("cat", "SLES", "", 0, 1),
        );
        let log = conn.run("cat /etc/os-release").await.expect("run ok");
        assert_eq!(log.stdout, "SLES");
        assert_eq!(log.runtime, 1);
    }

    #[tokio::test]
    async fn commands_are_recorded_in_order() {
        let mut conn = MockConnection::new("h1");
        conn.run("a").await.expect("a");
        conn.run("b").await.expect("b");
        conn.run("c").await.expect("c");
        assert_eq!(conn.commands(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn scripted_timeout_surfaces_host_error() {
        let mut conn = MockConnection::new("h1").with_timeout("sleep 999");
        let err = conn.run("sleep 999").await.expect_err("should time out");
        assert!(matches!(err, HostError::Timeout { command } if command == "sleep 999"));
        // The command is still recorded even though it failed.
        assert_eq!(conn.commands(), ["sleep 999"]);
    }

    #[tokio::test]
    async fn close_marks_inactive_and_closed() {
        let mut conn = MockConnection::new("h1");
        assert!(conn.is_active());
        assert!(!conn.is_closed());
        conn.close().await.expect("close ok");
        assert!(!conn.is_active());
        assert!(conn.is_closed());
    }

    #[tokio::test]
    async fn inactive_builder_reports_not_active() {
        let conn = MockConnection::new("h1").inactive();
        assert!(!conn.is_active());
    }

    #[tokio::test]
    async fn with_failing_close_returns_err() {
        let mut conn = MockConnection::new("h1").with_failing_close();
        let err = conn.close().await.expect_err("close should fail");
        assert!(matches!(err, HostError::Connect { host, .. } if host == "h1"));
        // A failed close does not mark the transport closed.
        assert!(!conn.is_closed());
    }

    #[tokio::test]
    async fn with_default_overrides_fallback() {
        let mut conn =
            MockConnection::new("h1").with_default(CommandLog::new("", "", "boom", 7, 0));
        let log = conn.run("anything").await.expect("run ok");
        assert_eq!(log.exitcode, 7);
        assert_eq!(log.stderr, "boom");
    }

    #[tokio::test]
    async fn usable_behind_boxed_trait_object() {
        let mut conn: Box<dyn Connection> = Box::new(MockConnection::new("h1"));
        let log = conn.run("whoami").await.expect("run ok");
        assert_eq!(log.exitcode, 0);
        conn.close().await.expect("close ok");
    }

    #[tokio::test]
    async fn reconnect_counts_and_reactivates() {
        let mut conn = MockConnection::new("h1").inactive();
        assert!(!conn.is_active());
        conn.reconnect(0, false).await.expect("reconnect ok");
        assert!(conn.is_active());
        assert_eq!(conn.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn failing_reconnect_surfaces_error() {
        let mut conn = MockConnection::new("h1").failing_reconnect();
        let err = conn.reconnect(0, false).await.expect_err("should fail");
        assert!(matches!(err, HostError::ReconnectFailed { host } if host == "h1"));
        assert_eq!(conn.reconnect_count(), 1);
    }

    #[tokio::test]
    async fn fire_and_forget_records_and_tears_down() {
        let mut conn = MockConnection::new("h1");
        conn.fire_and_forget("reboot").await.expect("dispatch ok");
        assert_eq!(conn.fired_commands(), ["reboot"]);
        assert!(!conn.is_active());
        assert!(conn.is_closed());
    }

    #[tokio::test]
    async fn sftp_put_get_are_recorded_in_order() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_put(Path::new("/tmp/a"), Path::new("/remote/a"))
            .await
            .expect("put ok");
        conn.sftp_get(Path::new("/remote/b"), Path::new("/tmp/b"))
            .await
            .expect("get ok");
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Put {
                    local: PathBuf::from("/tmp/a"),
                    remote: PathBuf::from("/remote/a"),
                },
                MockSftpOp::Get {
                    remote: PathBuf::from("/remote/b"),
                    local: PathBuf::from("/tmp/b"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn sftp_put_bytes_records_len_and_remote() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_put_bytes(b"payload", Path::new("/remote/a"))
            .await
            .expect("put bytes ok");
        assert_eq!(
            conn.sftp_ops(),
            [MockSftpOp::PutBytes {
                len: b"payload".len(),
                remote: PathBuf::from("/remote/a"),
            }]
        );
    }

    #[tokio::test]
    async fn sftp_put_failure_knob_errors_but_still_records() {
        let mut conn = MockConnection::new("h1").with_sftp_put_failure("disk full");
        let err = conn
            .sftp_put(Path::new("/tmp/a"), Path::new("/remote/a"))
            .await
            .expect_err("should fail");
        assert!(matches!(err, HostError::Sftp { .. }));
        let err = conn
            .sftp_put_bytes(b"payload", Path::new("/remote/b"))
            .await
            .expect_err("should fail");
        assert!(matches!(err, HostError::Sftp { .. }));
        // The op is recorded before failing, so a caller can still inspect it.
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Put {
                    local: PathBuf::from("/tmp/a"),
                    remote: PathBuf::from("/remote/a"),
                },
                MockSftpOp::PutBytes {
                    len: b"payload".len(),
                    remote: PathBuf::from("/remote/b"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn sftp_listdir_returns_scripted_entries() {
        let mut conn = MockConnection::new("h1").with_listing("/var/log", ["a.log", "b.log"]);
        let entries = conn.sftp_listdir(Path::new("/var/log")).await.expect("ok");
        assert_eq!(entries, ["a.log", "b.log"]);
        // Unscripted paths list empty, not error.
        let empty = conn.sftp_listdir(Path::new("/nope")).await.expect("ok");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn sftp_open_returns_scripted_bytes_or_errors() {
        let mut conn = MockConnection::new("h1").with_file("/etc/os-release", b"SLES".to_vec());
        let bytes = conn
            .sftp_open(Path::new("/etc/os-release"))
            .await
            .expect("ok");
        assert_eq!(bytes, b"SLES");
        let err = conn
            .sftp_open(Path::new("/missing"))
            .await
            .expect_err("should error");
        assert!(matches!(err, HostError::SftpNotFound { .. }));
    }

    #[tokio::test]
    async fn sftp_readlink_returns_scripted_target() {
        let mut conn = MockConnection::new("h1").with_link("/link", "/target");
        let target = conn.sftp_readlink(Path::new("/link")).await.expect("ok");
        assert_eq!(target, "/target");
        assert!(conn.sftp_readlink(Path::new("/nope")).await.is_err());
    }

    #[tokio::test]
    async fn sftp_write_creates_and_is_readable() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_write(Path::new("/var/lock/mtui.lock"), b"ts:user:1", false)
            .await
            .expect("write ok");
        let back = conn
            .sftp_open(Path::new("/var/lock/mtui.lock"))
            .await
            .expect("read ok");
        assert_eq!(back, b"ts:user:1");
    }

    #[tokio::test]
    async fn sftp_write_exclusive_collides_when_present() {
        let mut conn = MockConnection::new("h1");
        // First exclusive create wins.
        conn.sftp_write(Path::new("/f"), b"first", true)
            .await
            .expect("first exclusive create wins");
        // A second exclusive create loses the race.
        let err = conn
            .sftp_write(Path::new("/f"), b"second", true)
            .await
            .expect_err("second exclusive create must collide");
        assert!(matches!(err, HostError::AlreadyExists { .. }));
        // The winner's bytes are preserved (loser did not clobber).
        assert_eq!(conn.file_contents("/f").as_deref(), Some(&b"first"[..]));
    }

    #[tokio::test]
    async fn sftp_write_overwrite_replaces_and_records_order() {
        let mut conn = MockConnection::new("h1").with_file("/f", b"old".to_vec());
        // Non-exclusive overwrite replaces existing contents.
        conn.sftp_write(Path::new("/f"), b"new", false)
            .await
            .expect("overwrite ok");
        assert_eq!(conn.file_contents("/f").as_deref(), Some(&b"new"[..]));
        assert_eq!(
            conn.sftp_ops(),
            [MockSftpOp::Write {
                path: PathBuf::from("/f"),
                exclusive: false,
            }]
        );
    }

    #[tokio::test]
    async fn sftp_append_creates_missing_then_extends_and_records() {
        let mut conn = MockConnection::new("h1");
        // Missing file is created by the first append.
        conn.sftp_append(Path::new("/log"), b"a\n")
            .await
            .expect("first append creates");
        // A second append extends at EOF, preserving the first entry.
        conn.sftp_append(Path::new("/log"), b"b\n")
            .await
            .expect("second append extends");
        assert_eq!(conn.file_contents("/log").as_deref(), Some(&b"a\nb\n"[..]));
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Append(PathBuf::from("/log")),
                MockSftpOp::Append(PathBuf::from("/log")),
            ]
        );
    }

    #[tokio::test]
    async fn sftp_append_scripted_error_propagates() {
        let mut conn = MockConnection::new("h1").with_sftp_append_error("/log");
        let err = conn
            .sftp_append(Path::new("/log"), b"x")
            .await
            .expect_err("scripted append failure propagates");
        assert!(matches!(err, HostError::Sftp { .. }));
        // The op is still recorded even though it failed.
        assert_eq!(conn.sftp_ops(), [MockSftpOp::Append(PathBuf::from("/log"))]);
    }

    #[tokio::test]
    async fn sftp_remove_rmdir_getfolder_recorded() {
        let mut conn = MockConnection::new("h1");
        conn.sftp_remove(Path::new("/f")).await.expect("ok");
        conn.sftp_rmdir(Path::new("/d")).await.expect("ok");
        conn.sftp_get_folder(Path::new("/rd"), Path::new("/ld"))
            .await
            .expect("ok");
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Remove(PathBuf::from("/f")),
                MockSftpOp::Rmdir(PathBuf::from("/d")),
                MockSftpOp::GetFolder {
                    remote: PathBuf::from("/rd"),
                    local: PathBuf::from("/ld"),
                },
            ]
        );
    }

    #[tokio::test]
    async fn sftp_session_batches_reads_and_counts_one_open() {
        // One `sftp_session()` open serves several reads; the per-read ops are
        // recorded through the shared log exactly as the per-op methods would.
        let mut conn = MockConnection::new("h1")
            .with_listing("/d", ["a", "b"])
            .with_file("/d/a", b"A".to_vec());
        {
            let mut sess = conn.sftp_session().await.expect("session opens");
            assert_eq!(
                sess.listdir(Path::new("/d")).await.expect("listdir"),
                ["a", "b"]
            );
            assert_eq!(sess.open(Path::new("/d/a")).await.expect("open"), b"A");
            sess.close().await.expect("close");
        }
        assert_eq!(conn.sftp_session_count(), 1);
        assert_eq!(
            conn.sftp_ops(),
            [
                MockSftpOp::Listdir(PathBuf::from("/d")),
                MockSftpOp::Open(PathBuf::from("/d/a")),
            ]
        );
    }

    #[tokio::test]
    async fn cloned_handle_shares_transport_state() {
        // `clone_box()` yields a handle that shares the SFTP session counter and
        // op log via `Arc` — the mock proxy for "reuses the same transport".
        // This is what lets a `TargetLock`/`PoolLock` built from a target's
        // clone be observed operating against the *same* connection state in
        // offline tests. (The real `SshConnection::clone_box` clones identity
        // with an empty handle and opens its own transport on first use; the
        // mock deliberately shares so lock behaviour stays observable.)
        let mut conn = MockConnection::new("h1").with_file("/f", b"x".to_vec());
        let mut clone = conn.clone_box();

        {
            let mut s1 = conn.sftp_session().await.expect("open on original");
            let _ = s1.open(Path::new("/f")).await;
        }
        {
            let mut s2 = clone.sftp_session().await.expect("open on clone");
            let _ = s2.open(Path::new("/f")).await;
        }
        // Both opens counted against the one shared counter.
        assert_eq!(conn.sftp_session_count(), 2);
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_records_spawn_and_serves_canned_output_then_eof() {
        let conn = MockConnection::new("h1").with_shell_output(b"hi".to_vec());
        let handle = conn.clone();
        let mut conn = conn;

        let mut ch = conn.shell(100, 30).await.expect("shell spawns");
        assert_eq!(handle.shell_spawns(), vec![(100, 30)]);

        let mut buf = [0u8; 8];
        let n = ch.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"hi");
        assert_eq!(ch.read(&mut buf).await.expect("eof"), 0);

        ch.write(b"q").await.expect("write");
        ch.resize(90, 20).await.expect("resize");
        ch.close().await.expect("close");
        assert_eq!(handle.shell_input(), b"q");
        assert_eq!(handle.shell_resizes(), vec![(90, 20)]);
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_read_carries_over_chunk_larger_than_buffer() {
        // A chunk larger than the read buffer is served in pieces across
        // successive reads (leftover carryover), never truncated — mirroring
        // paramiko's `recv(n)` and the real SSH channel, so no PTY bytes are
        // lost on a short buffer.
        let mut conn = MockConnection::new("h1").with_shell_output(b"abcdef".to_vec());
        let mut ch = conn.shell(80, 24).await.expect("spawn");
        let mut buf = [0u8; 3];

        let n = ch.read(&mut buf).await.expect("read 1");
        assert_eq!(&buf[..n], b"abc");
        let n = ch.read(&mut buf).await.expect("read 2 drains leftover");
        assert_eq!(&buf[..n], b"def");
        assert_eq!(ch.read(&mut buf).await.expect("eof"), 0);
    }
}
