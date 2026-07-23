//! Remote lock management for target hosts.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/locks.py`. There are **two remote
//! locks** with the same wire format but different scope and ownership rules:
//!
//! * [`TargetLock`] — the *operation* lock at `/var/lock/mtui.lock`. It guards
//!   serialized `zypper` transactions (`zypper` holds a system-wide lock, so
//!   mtui must not run two updates against one host at once). Ownership is
//!   **PID-based**: the lock is "mine" only when this exact process took it, so
//!   one user running several `mtui` instances against the same host does not
//!   collide with themselves.
//! * [`PoolLock`] — the *pool-claim* lock at `/var/lock/mtui-pool.lock`. It
//!   marks a reference host as claimed by a particular template (RRID) during
//!   host arbitration, so a different user/template does not connect to a host
//!   already taken. Ownership is **RRID-based** and ignores the PID, because a
//!   pool claim outlives the process that took it (a tester may reconnect from
//!   a fresh `mtui` invocation and must still be recognised as the owner).
//!
//! ## Wire format (contract)
//!
//! Both locks serialize one line, `timestamp:user:pid[:comment]`, into their
//! respective `/var/lock/*.lock` file (see [`RemoteLock::to_lockfile`]). This
//! is a **cross-implementation contract**: a Python `mtui` and this Rust `mtui`
//! may share a host fleet, so the byte layout must match upstream exactly. The
//! `comment` field keeps any embedded colons (parsed with a 3-way split); a
//! `PoolLock` stores `mtui pool <RRID> [<owner>]` there.
//!
//! ## Atomicity
//!
//! [`TargetLock::lock`] first attempts an **atomic exclusive create**
//! ([`Connection::sftp_write`] with `exclusive = true`, paramiko mode `"x"` →
//! `O_CREAT | O_EXCL`): exactly one of two racing processes wins, and the loser
//! reconciles (re-stamp if it's ours, reap if stale, wait if configured, else
//! refuse). This closes the read-then-write TOCTOU the old "stat then write"
//! sequence had.
//!
//! ## Deviations from upstream
//!
//! * All I/O is `async` (the connection layer is async-native).
//! * The wait queue takes an injected [`Clock`] instead of monkeypatching
//!   `time.sleep`, so the polling loop is unit-testable without real sleeps.
//! * `TargetLockedError` is folded into [`HostError::TargetLocked`] rather than
//!   being a distinct exception type.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use mtui_config::Config;

use crate::connection::Connection;
use crate::error::{HostError, Result};

/// A source of wall-clock time and async sleeping, injected so the lock
/// wait-queue is testable without real delays.
///
/// The default [`SystemClock`] uses `SystemTime` and `tokio::time::sleep`;
/// tests substitute a fake clock that advances instantly.
#[async_trait::async_trait]
pub trait Clock: Send + Sync {
    /// Current Unix time in seconds.
    fn now_unix(&self) -> u64;
    /// A monotonic instant in seconds (for deadline math). Need not be tied to
    /// `now_unix`; only differences are used.
    fn monotonic(&self) -> f64;
    /// Sleep for `secs` seconds.
    async fn sleep(&self, secs: f64);
}

/// The production [`Clock`]: real system time + `tokio` sleep.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[async_trait::async_trait]
impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn monotonic(&self) -> f64 {
        // Instant has no absolute epoch; use a process-lifetime reference.
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(std::time::Instant::now);
        start.elapsed().as_secs_f64()
    }

    async fn sleep(&self, secs: f64) {
        if secs > 0.0 {
            tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
        }
    }
}

/// The parsed state of a remote lock line.
///
/// An empty [`user`](Self::user) means "no lock". Serializes to
/// `timestamp:user:pid[:comment]` via [`to_lockfile`](Self::to_lockfile).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteLock {
    /// The user who took the lock.
    pub user: String,
    /// The Unix timestamp (seconds, as a string) when the lock was taken.
    pub timestamp: String,
    /// The PID of the process that took the lock.
    pub pid: u32,
    /// Optional comment. A non-empty comment marks an *exclusive* lock; a
    /// [`PoolLock`] stores `mtui pool <RRID> [<owner>]` here.
    pub comment: String,
}

/// A resolved snapshot of a host's lock ownership, produced by
/// [`Target::lock_status`](crate::Target::lock_status) and forwarded by the
/// [`Reporter`](crate::Reporter)/[`HostsGroup`](crate::HostsGroup) lock sinks to
/// the display layer.
///
/// The upstream lock accessors are async `&mut self`; the resolving code does
/// that I/O once and hands these already-resolved (sync) values downstream so
/// the display stays sync and snapshot-testable. Mirrors the fields the upstream
/// `display.list_locks` reads off the lock object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockRow {
    /// Whether the host is currently locked/claimed.
    pub is_locked: bool,
    /// Whether the lock belongs to the current owner (renders as "me").
    pub is_mine: bool,
    /// The lock owner (ignored by the display when `is_mine`).
    pub locked_by: String,
    /// Human-readable lock timestamp.
    pub time: String,
    /// The claim's trailing detail line: the free-form comment for an operation
    /// lock, or the owning template's RRID for a pool claim (see
    /// [`Target::lock_status`](crate::Target::lock_status)). Empty when there is
    /// none.
    pub comment: String,
}

/// A single-read view of a lock's on-disk state plus derived ownership.
///
/// Produced by [`TargetLock::snapshot`] / [`PoolLock::snapshot`] with **exactly
/// one** remote read, so [`Target::lock_status`](crate::Target::lock_status) can
/// derive every displayed field without re-reading the lockfile per field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockSnapshot {
    /// The parsed on-disk lock (empty [`user`](RemoteLock::user) ⇒ unlocked).
    pub(crate) lock: RemoteLock,
    /// Whether the lock belongs to the current owner. `false` when unlocked
    /// (the accessor's "not locked" error is not propagated here).
    pub(crate) is_mine: bool,
    /// The owning template's RRID, for a [`PoolLock`] claim only; empty for an
    /// operation-lock snapshot or a non-pool comment.
    pub(crate) rrid: String,
}

impl RemoteLock {
    /// Serializes to a lockfile line: `timestamp:user:pid[:comment]`.
    ///
    /// The comment field is only appended when non-empty, matching upstream.
    #[must_use]
    pub fn to_lockfile(&self) -> String {
        let mut xs = vec![
            self.timestamp.clone(),
            self.user.clone(),
            self.pid.to_string(),
        ];
        if !self.comment.is_empty() {
            xs.push(self.comment.clone());
        }
        xs.join(":")
    }

    /// Parses a lockfile line into a [`RemoteLock`].
    ///
    /// An empty line yields the empty (unlocked) state. The line is split at
    /// most 3 times so a comment keeps any embedded colons. Fewer than three
    /// fields is a malformed lockfile.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Sftp`] with a "weird format" reason when the line
    /// has fewer than three colon-separated fields, mirroring upstream's
    /// `ValueError("got weird format in lockfile")`.
    pub fn from_lockfile(line: &str) -> Result<Self> {
        if line.is_empty() {
            return Ok(Self::default());
        }
        let line = line.trim();
        // splitn(4) mirrors Python's split(":", 3): at most 4 parts, so the
        // 4th (comment) retains embedded colons.
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() < 3 {
            return Err(HostError::Sftp {
                host: String::new(),
                reason: "got weird format in lockfile".to_owned(),
            });
        }
        let pid = parts[2].parse::<u32>().map_err(|_| HostError::Sftp {
            host: String::new(),
            reason: format!("got weird format in lockfile: bad pid {:?}", parts[2]),
        })?;
        Ok(Self {
            timestamp: parts[0].to_owned(),
            user: parts[1].to_owned(),
            pid,
            comment: parts.get(3).map_or(String::new(), |c| (*c).to_owned()),
        })
    }

    /// A formatted lock-creation time, or `"unknown"` when the stored timestamp
    /// is missing/malformed. Shared by [`TargetLock::time`] and the one-read
    /// snapshot path so both render identically.
    #[must_use]
    pub(crate) fn display_time(&self) -> String {
        match self.timestamp.parse::<i64>() {
            Ok(ts) => format_utc(ts),
            Err(_) => "unknown".to_owned(),
        }
    }

    /// Human-readable "locked by <user> (<comment>)." string.
    #[must_use]
    fn describe(&self) -> String {
        if self.comment.is_empty() {
            format!("locked by {}.", self.user)
        } else {
            format!("locked by {} ({}).", self.user, self.comment)
        }
    }
}

impl std::fmt::Display for RemoteLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Manages the operation lock (`/var/lock/mtui.lock`) for a single target.
///
/// Ownership is PID-based (see the module docs). Drives the injected
/// [`Connection`] for all remote I/O and an injected [`Clock`] for the wait
/// queue.
pub struct TargetLock<C: Clock = SystemClock> {
    connection: Box<dyn Connection>,
    clock: C,
    i_am_user: String,
    i_am_pid: u32,
    lock_reap_stale: bool,
    lock_stale_age: u64,
    lock_wait: u64,
    lock_wait_poll: u64,
    lock: RemoteLock,
    /// The remote lockfile this instance manages. Defaults to
    /// [`TARGET_LOCK_PATH`]; [`PoolLock`] builds its inner lock over
    /// [`POOL_LOCK_PATH`] instead, so the shared lock/unlock/load machinery
    /// touches the correct file (upstream expresses this via `PoolLock`
    /// overriding `filename()`).
    path: PathBuf,
}

/// The default operation-lock path, `/var/lock/mtui.lock` — a cross-mtui
/// contract (a Python and a Rust mtui may share a host fleet).
pub const TARGET_LOCK_PATH: &str = "/var/lock/mtui.lock";

/// The default pool-claim-lock path, `/var/lock/mtui-pool.lock`.
pub(crate) const POOL_LOCK_PATH: &str = "/var/lock/mtui-pool.lock";

impl TargetLock<SystemClock> {
    /// Builds a `TargetLock` over `connection` using config-derived identity and
    /// reap/wait behaviour, with the real [`SystemClock`].
    #[must_use]
    pub fn new(connection: Box<dyn Connection>, config: &Config) -> Self {
        Self::with_clock(connection, config, SystemClock)
    }
}

impl<C: Clock> TargetLock<C> {
    /// Builds a `TargetLock` with an explicit [`Clock`] (for tests).
    #[must_use]
    fn with_clock(connection: Box<dyn Connection>, config: &Config, clock: C) -> Self {
        Self::with_clock_and_path(connection, config, clock, PathBuf::from(TARGET_LOCK_PATH))
    }

    /// Builds a `TargetLock` with an explicit [`Clock`] and lockfile `path`.
    ///
    /// The path seam lets [`PoolLock`] reuse the whole lock/unlock/load
    /// machinery over [`POOL_LOCK_PATH`] instead of the operation-lock path.
    #[must_use]
    fn with_clock_and_path(
        connection: Box<dyn Connection>,
        config: &Config,
        clock: C,
        path: PathBuf,
    ) -> Self {
        Self {
            connection,
            clock,
            i_am_user: config.session_user.clone(),
            i_am_pid: std::process::id(),
            lock_reap_stale: config.lock_reap_stale,
            lock_stale_age: config.lock_stale_age,
            lock_wait: config.lock_wait,
            lock_wait_poll: config.lock_wait_poll,
            lock: RemoteLock::default(),
            path,
        }
    }

    /// The lockfile path this lock manages. Set at construction; defaults to
    /// [`TARGET_LOCK_PATH`], overridden to [`POOL_LOCK_PATH`] for [`PoolLock`].
    fn filename(&self) -> PathBuf {
        self.path.clone()
    }

    /// The remote hostname (for messages).
    fn hostname(&self) -> String {
        self.connection.hostname().to_owned()
    }

    /// Loads the lock state from the remote host into [`Self::lock`].
    ///
    /// **Fails closed**: only a genuinely *missing* lockfile
    /// ([`HostError::SftpNotFound`]) resets to the empty (unlocked) state,
    /// mirroring upstream's `errno.ENOENT` branch. Every other error — a
    /// permission-denied read, a transport blip, a truncated/garbled read — is
    /// propagated, because reporting "unlocked" for an *unknown* state would let
    /// us claim a host another owner already holds. (Upstream re-raises any
    /// non-`ENOENT` `OSError` for the same reason.)
    async fn load(&mut self) -> Result<()> {
        self.lock = RemoteLock::default();
        let path = self.filename();
        match self.connection.sftp_open(&path).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let line = text.lines().next().unwrap_or("");
                self.lock = RemoteLock::from_lockfile(line)?;
                Ok(())
            }
            Err(HostError::SftpNotFound { .. }) => {
                // A truly absent lockfile means "unlocked" (upstream ENOENT).
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Whether the host is currently locked (by anyone).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub async fn is_locked(&mut self) -> Result<bool> {
        self.load().await?;
        Ok(!self.lock.user.is_empty())
    }

    /// The age of the current lock in seconds, or `None` when unlocked or the
    /// stored timestamp is missing/malformed (callers treat such a lock as
    /// "leave it alone").
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    async fn age_seconds(&mut self) -> Result<Option<u64>> {
        self.load().await?;
        if self.lock.user.is_empty() || self.lock.timestamp.is_empty() {
            return Ok(None);
        }
        match self.lock.timestamp.parse::<u64>() {
            Ok(ts) => Ok(Some(self.clock.now_unix().saturating_sub(ts))),
            Err(_) => Ok(None),
        }
    }

    /// Force-removes the lock if it is older than the configured stale age.
    ///
    /// Controlled by `lock_reap_stale` (default on) and `lock_stale_age`
    /// (default 86400s); a non-positive age disables reaping. Applies to any
    /// lock, including exclusive (commented) ones and foreign locks, since a
    /// sufficiently old lock is almost always abandoned.
    ///
    /// # Errors
    /// Propagates an SFTP error from the load/unlock path.
    pub(crate) async fn reap_if_stale(&mut self) -> Result<bool> {
        if !self.lock_reap_stale || self.lock_stale_age == 0 {
            return Ok(false);
        }
        let Some(age) = self.age_seconds().await? else {
            return Ok(false);
        };
        if age <= self.lock_stale_age {
            return Ok(false);
        }
        tracing::warn!(
            host = %self.hostname(), user = %self.lock.user, hours = age / 3600,
            "removing stale lock"
        );
        self.unlock(true).await?;
        Ok(true)
    }

    /// Claim the lock without raising when it is busy.
    ///
    /// Returns `true` when the lock is now ours (host free, already ours, or a
    /// stale lock reaped); `false` when it is held by someone else.
    ///
    /// # Errors
    /// Propagates an SFTP error from the underlying calls.
    pub async fn try_claim(&mut self, comment: &str) -> Result<bool> {
        if self.is_locked().await? && !self.is_mine()? && !self.reap_if_stale().await? {
            return Ok(false);
        }
        match self.lock(comment).await {
            Ok(()) => Ok(true),
            Err(HostError::TargetLocked(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Queue for a busy lock up to `lock_wait` seconds, polling every
    /// `lock_wait_poll` seconds. Returns `true` when it became free/ours/reaped
    /// within the budget (the caller may claim it), `false` otherwise. With
    /// `lock_wait <= 0` this is a no-op returning `false` (fail-fast).
    async fn wait_for_lock(&mut self) -> Result<bool> {
        let wait = self.lock_wait;
        if wait == 0 {
            return Ok(false);
        }
        let poll = self.lock_wait_poll.max(1) as f64;
        let deadline = self.clock.monotonic() + wait as f64;
        tracing::warn!(
            host = %self.hostname(), user = %self.lock.user, wait,
            "locked; waiting for it to free"
        );
        loop {
            if self.reap_if_stale().await? {
                return Ok(true);
            }
            let remaining = (deadline - self.clock.monotonic()).max(0.0);
            self.clock.sleep(poll.min(remaining)).await;
            if !self.is_locked().await? || self.is_mine()? {
                return Ok(true);
            }
            if self.clock.monotonic() >= deadline {
                tracing::warn!(
                    host = %self.hostname(), user = %self.lock.user, wait,
                    "still locked after wait; giving up"
                );
                return Ok(false);
            }
        }
    }

    /// The maximum number of atomic-create retries during reconciliation, so a
    /// pathological create/reap/create ping-pong with a competing racer cannot
    /// spin forever. On exhaustion the caller reports the host as locked.
    const RECONCILE_RETRIES: u32 = 8;

    /// Locks the target.
    ///
    /// Attempts an atomic exclusive create first; on collision, reconciles
    /// (re-stamp if ours, reap if stale, wait if configured) or refuses.
    ///
    /// **Race-safe**: a foreign lock is never blind-overwritten. The only
    /// non-exclusive (truncating) write is a re-stamp of a lock this process
    /// *already owns* — clobbering "the winner" there means clobbering
    /// ourselves. When wait/reap frees the host, the acquisition is a **retried
    /// atomic exclusive create**, so a racer that took the lock in the TOCTOU
    /// window between our last check and the write wins and we reconcile again
    /// rather than stomping their line.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the host is held by another
    /// owner and cannot be acquired, or an SFTP error from the read/write path.
    pub async fn lock(&mut self, comment: &str) -> Result<()> {
        let rl = RemoteLock {
            user: self.i_am_user.clone(),
            timestamp: self.clock.now_unix().to_string(),
            pid: self.i_am_pid,
            comment: comment.to_owned(),
        };
        let path = self.filename();
        let line = rl.to_lockfile();

        for _ in 0..Self::RECONCILE_RETRIES {
            // 1) Atomic exclusive create: on a free host exactly one racer wins.
            match self
                .connection
                .sftp_write(&path, line.as_bytes(), true)
                .await
            {
                Ok(()) => {
                    self.lock = rl;
                    return Ok(());
                }
                Err(HostError::AlreadyExists { .. }) => {
                    // Fall through to reconciliation.
                }
                Err(e) => return Err(e),
            }

            // 2) The file exists. Load it (fail-closed) and decide.
            self.load().await?;
            if self.lock.user.is_empty() {
                // Freed between the create and the load — retry the create.
                continue;
            }
            if self.is_mine()? {
                // Legitimate re-stamp of our own lock (possibly a new comment).
                self.connection
                    .sftp_write(&path, line.as_bytes(), false)
                    .await?;
                self.lock = rl;
                return Ok(());
            }
            // Foreign lock: reap-if-stale / wait may free it, then we retry the
            // atomic create (never a blind overwrite of a foreign line).
            if self.wait_for_lock().await? {
                continue;
            }
            return Err(HostError::TargetLocked(self.locked_by_msg().await?));
        }

        // Exhausted retries: treat as contended and fail closed.
        Err(HostError::TargetLocked(self.locked_by_msg().await?))
    }

    /// A "locked by" message suitable for display.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn locked_by_msg(&mut self) -> Result<String> {
        self.load().await?;
        Ok(format!("{} is {}", self.hostname(), self.lock))
    }

    /// The user who currently holds the lock (empty when unlocked).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn locked_by(&mut self) -> Result<String> {
        self.load().await?;
        Ok(self.lock.user.clone())
    }

    /// The comment on the current lock (empty when none/unlocked).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn comment(&mut self) -> Result<String> {
        self.load().await?;
        Ok(self.lock.comment.clone())
    }

    /// A formatted lock-creation time, or `"unknown"` when the stored timestamp
    /// is missing/malformed (must not raise — this is called while reporting a
    /// foreign lock during acquisition).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn time(&mut self) -> Result<String> {
        self.load().await?;
        Ok(self.lock.display_time())
    }

    /// Unlocks the target.
    ///
    /// With `force = false` a foreign lock is refused; with `force = true` any
    /// owner's lock is removed. A no-op on an unlocked host.
    ///
    /// **Fails closed on removal**: an already-missing lockfile
    /// ([`HostError::SftpNotFound`], upstream `ENOENT`) counts as released, but
    /// any other remove failure (permission, transport) propagates and leaves
    /// the in-memory lock state intact — the caller must not believe it released
    /// a lock it did not.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the lock is foreign and not
    /// forced, or an SFTP/transport error from the remove path (other than
    /// "already gone").
    pub async fn unlock(&mut self, force: bool) -> Result<()> {
        if !self.is_locked().await? {
            return Ok(());
        }
        if !self.is_mine()? && !force {
            return Err(HostError::TargetLocked(self.locked_by_msg().await?));
        }
        let path = self.filename();
        match self.connection.sftp_remove(&path).await {
            Ok(()) => {}
            Err(HostError::SftpNotFound { .. }) => {
                // Already gone — treat as released (upstream ENOENT branch).
                tracing::debug!(host = %self.hostname(), "lockfile already gone");
            }
            Err(e) => {
                // A permission/transport failure did NOT remove the lock; do not
                // pretend we released it (fail closed — leave self.lock intact).
                return Err(e);
            }
        }
        self.lock = RemoteLock::default();
        Ok(())
    }

    /// Whether the currently-loaded lock is owned by this process.
    ///
    /// PID-based: mine only when both the user and the PID match. Call after a
    /// method that loads the lock (e.g. [`is_locked`](Self::is_locked)).
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] with a "not locked" reason when no
    /// lock is loaded (mirrors upstream's `RuntimeError("not locked")`).
    pub(crate) fn is_mine(&self) -> Result<bool> {
        if self.lock.user.is_empty() {
            return Err(HostError::TargetLocked("not locked".to_owned()));
        }
        if self.lock.user != self.i_am_user {
            return Ok(false);
        }
        Ok(self.lock.pid == self.i_am_pid)
    }

    /// Read-only view of the currently-loaded lock (for tests / callers).
    #[must_use]
    fn current(&self) -> &RemoteLock {
        &self.lock
    }

    /// Resolves the lock's on-disk state and ownership with a **single** remote
    /// read, for lock reporting ([`Target::lock_status`](crate::Target::lock_status)).
    ///
    /// One [`load`](Self::load); `is_mine` is derived from the freshly-loaded
    /// cache (an unlocked lock is not "mine" and does not raise). Leaves the
    /// per-field async accessors for the wait/claim/unlock callers, which
    /// intentionally re-read between retries.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn snapshot(&mut self) -> Result<LockSnapshot> {
        self.load().await?;
        let is_mine = self.is_mine().unwrap_or(false);
        Ok(LockSnapshot {
            lock: self.lock.clone(),
            is_mine,
            rrid: String::new(),
        })
    }
}

/// The pool-claim lock (`/var/lock/mtui-pool.lock`), separate from the
/// operation lock, with RRID-based ownership.
///
/// See the module docs: a pool claim marks a host as taken by a template
/// (RRID). Ownership ignores the PID and compares the RRID recorded in the
/// comment (`mtui pool <RRID> [<owner>]`) against this session's RRID, so a
/// tester reconnecting from a fresh process is still the owner. With no session
/// RRID, ownership degrades to user-only.
pub struct PoolLock<C: Clock = SystemClock> {
    inner: TargetLock<C>,
    i_am_rrid: String,
    /// Force-reap a pool claim older than [`Self::pool_stale_age`] on claim
    /// (`pool_reap_stale`). The pool-claim analogue of the operation lock's
    /// `lock_reap_stale`, kept separate because the inner [`TargetLock`]'s
    /// reap fields govern the *operation* lock's stale age, not the pool claim's.
    pool_reap_stale: bool,
    /// Age (seconds) beyond which a pool claim is reapable (`pool_stale_age`);
    /// `0` disables pool-claim reaping.
    pool_stale_age: u64,
}

impl PoolLock<SystemClock> {
    /// Builds a `PoolLock` with the real [`SystemClock`].
    #[must_use]
    pub(crate) fn new(
        connection: Box<dyn Connection>,
        config: &Config,
        rrid: impl Into<String>,
    ) -> Self {
        Self::with_clock(connection, config, rrid, SystemClock)
    }
}

impl<C: Clock> PoolLock<C> {
    /// Builds a `PoolLock` with an explicit [`Clock`] (for tests).
    #[must_use]
    fn with_clock(
        connection: Box<dyn Connection>,
        config: &Config,
        rrid: impl Into<String>,
        clock: C,
    ) -> Self {
        Self {
            inner: TargetLock::with_clock_and_path(
                connection,
                config,
                clock,
                PathBuf::from(POOL_LOCK_PATH),
            ),
            i_am_rrid: rrid.into(),
            pool_reap_stale: config.pool_reap_stale,
            pool_stale_age: config.pool_stale_age,
        }
    }

    /// Extracts the RRID from a `mtui pool <RRID> [<owner>]` comment, or `""`
    /// when the comment is not a pool comment.
    #[must_use]
    fn rrid_of(comment: &str) -> String {
        let parts: Vec<&str> = comment.split_whitespace().collect();
        if parts.len() >= 3 && parts[0] == "mtui" && parts[1] == "pool" {
            parts[2].to_owned()
        } else {
            String::new()
        }
    }

    /// Whether the pool lock belongs to this template + user.
    ///
    /// RRID-based (ignores PID). Falls back to user-only when this session has
    /// no RRID.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] with a "not locked" reason when no
    /// lock is loaded.
    fn is_mine(&self) -> Result<bool> {
        let lock = self.inner.current();
        if lock.user.is_empty() {
            return Err(HostError::TargetLocked("not locked".to_owned()));
        }
        if lock.user != self.inner.i_am_user {
            return Ok(false);
        }
        if self.i_am_rrid.is_empty() {
            return Ok(true);
        }
        Ok(Self::rrid_of(&lock.comment) == self.i_am_rrid)
    }

    /// The pool lockfile path.
    #[must_use]
    fn filename(&self) -> PathBuf {
        PathBuf::from(POOL_LOCK_PATH)
    }

    /// Resolves the pool claim's on-disk state, RRID-based ownership, and owning
    /// RRID with a **single** remote read, for lock reporting
    /// ([`Target::lock_status`](crate::Target::lock_status)).
    ///
    /// One inner [`load`](TargetLock::load); `is_mine` uses the pool RRID rule
    /// against the freshly-loaded cache (an unclaimed lock is not "mine" and
    /// does not raise), and `rrid` is parsed from the same cached comment.
    ///
    /// # Errors
    /// Propagates an SFTP error from the load path.
    pub(crate) async fn snapshot(&mut self) -> Result<LockSnapshot> {
        self.inner.load().await?;
        let is_mine = self.is_mine().unwrap_or(false);
        let lock = self.inner.current().clone();
        let rrid = Self::rrid_of(&lock.comment);
        Ok(LockSnapshot {
            lock,
            is_mine,
            rrid,
        })
    }

    /// Sets the ownership RRID (the report layer pushes this down after the
    /// claim is built; see [`Target::set_rrid`](crate::Target::set_rrid)).
    pub(crate) fn set_rrid(&mut self, rrid: impl Into<String>) {
        self.i_am_rrid = rrid.into();
    }

    /// Whether the pool claim is currently held (by anyone).
    ///
    /// Delegates to the inner [`TargetLock`], which reads the pool lockfile.
    ///
    /// # Errors
    /// Propagates an SFTP error from the load path.
    async fn is_locked(&mut self) -> Result<bool> {
        self.inner.is_locked().await
    }

    /// Claims the pool lock, recording `comment` (the
    /// `mtui pool <RRID> [<owner>]` stamp).
    ///
    /// Delegates to the inner [`TargetLock::lock`], which writes the pool
    /// lockfile atomically.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the host is claimed by another
    /// owner, or an SFTP error from the write path.
    async fn lock(&mut self, comment: &str) -> Result<()> {
        self.inner.lock(comment).await
    }

    /// Force-removes the pool claim if it is older than the configured pool
    /// stale age.
    ///
    /// The pool-claim analogue of [`TargetLock::reap_if_stale`], gated by
    /// `pool_reap_stale` (default on) and `pool_stale_age` (default 86400s; `0`
    /// disables reaping). Pool ownership is RRID-based, so a foreign claim's PID
    /// disappearing never frees it — an orphan left by an uncatchable exit
    /// (SIGKILL / panic / power loss) would otherwise block arbitration until a
    /// manual `unlock -f -p`. This reaps any pool claim older than the TTL,
    /// regardless of owner, via a forced [`unlock`](Self::unlock).
    ///
    /// # Errors
    /// Propagates an SFTP error from the load/unlock path.
    async fn reap_if_stale(&mut self) -> Result<bool> {
        if !self.pool_reap_stale || self.pool_stale_age == 0 {
            return Ok(false);
        }
        let Some(age) = self.inner.age_seconds().await? else {
            return Ok(false);
        };
        if age <= self.pool_stale_age {
            return Ok(false);
        }
        tracing::warn!(
            host = %self.inner.hostname(),
            user = %self.inner.current().user,
            hours = age / 3600,
            "removing stale pool claim"
        );
        self.unlock(true).await?;
        Ok(true)
    }

    /// Claims the pool lock without raising when it is already claimed.
    ///
    /// The non-raising counterpart to [`lock`](Self::lock), used by host
    /// arbitration. Ownership is RRID-based: a claim recording *our* RRID (even
    /// from another process) counts as ours. A foreign claim older than
    /// `pool_stale_age` is force-reaped first (see
    /// [`reap_if_stale`](Self::reap_if_stale)), the only automatic recovery for a
    /// claim orphaned by an uncatchable exit.
    ///
    /// # Errors
    /// Propagates an SFTP error from the underlying calls.
    pub(crate) async fn try_claim(&mut self, comment: &str) -> Result<bool> {
        if self.is_locked().await? && !self.is_mine()? && !self.reap_if_stale().await? {
            return Ok(false);
        }
        match self.lock(comment).await {
            Ok(()) => Ok(true),
            Err(HostError::TargetLocked(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Releases the pool claim.
    ///
    /// Unlike [`TargetLock::unlock`] (PID-based ownership), a foreign claim is
    /// decided by **RRID** ([`is_mine`](Self::is_mine)): with `force = false` a
    /// claim owned by another template is refused; with `force = true` any
    /// claim is removed. A no-op on an unclaimed host.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the claim is foreign and not
    /// forced, or an SFTP error from the remove path.
    pub(crate) async fn unlock(&mut self, force: bool) -> Result<()> {
        if !self.is_locked().await? {
            return Ok(());
        }
        if !self.is_mine()? && !force {
            return Err(HostError::TargetLocked(self.inner.locked_by_msg().await?));
        }
        let path = self.filename();
        match self.inner.connection.sftp_remove(&path).await {
            Ok(()) => {}
            Err(HostError::SftpNotFound { .. }) => {
                tracing::debug!("pool lockfile already gone");
            }
            Err(e) => {
                // The claim was not removed; do not mark it released.
                return Err(e);
            }
        }
        self.inner.lock = RemoteLock::default();
        Ok(())
    }
}

/// Converts a Unix timestamp to `%A, %d.%m.%Y %H:%M UTC`.
///
/// Self-contained civil-date arithmetic (no external date crate). Display-only;
/// the wire contract lives in [`RemoteLock::to_lockfile`], not here.
fn format_utc(ts: i64) -> String {
    const WEEKDAYS: [&str; 7] = [
        "Thursday", // 1970-01-01 was a Thursday (day 0)
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    let days = ts.div_euclid(86400);
    let secs_of_day = ts.rem_euclid(86400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let (y, m, d) = civil_from_days(days);
    format!("{weekday}, {d:02}.{m:02}.{y:04} {hour:02}:{minute:02} UTC")
}

/// Howard Hinnant's `civil_from_days` algorithm: days-since-epoch → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::MockConnection;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    // --- RemoteLock ---------------------------------------------------------

    #[test]
    fn default_is_empty() {
        let rl = RemoteLock::default();
        assert_eq!(rl.user, "");
        assert_eq!(rl.timestamp, "");
        assert_eq!(rl.pid, 0);
        assert_eq!(rl.comment, "");
    }

    #[test]
    fn to_lockfile_without_comment() {
        let rl = RemoteLock {
            timestamp: "1700000000".into(),
            user: "testuser".into(),
            pid: 12345,
            comment: String::new(),
        };
        assert_eq!(rl.to_lockfile(), "1700000000:testuser:12345");
    }

    #[test]
    fn to_lockfile_with_comment() {
        let rl = RemoteLock {
            timestamp: "1700000000".into(),
            user: "testuser".into(),
            pid: 12345,
            comment: "update in progress".into(),
        };
        assert_eq!(
            rl.to_lockfile(),
            "1700000000:testuser:12345:update in progress"
        );
    }

    #[test]
    fn from_lockfile_empty() {
        let rl = RemoteLock::from_lockfile("").unwrap();
        assert_eq!(rl.user, "");
        assert_eq!(rl.pid, 0);
    }

    #[test]
    fn from_lockfile_basic() {
        let rl = RemoteLock::from_lockfile("1700000000:testuser:12345").unwrap();
        assert_eq!(rl.timestamp, "1700000000");
        assert_eq!(rl.user, "testuser");
        assert_eq!(rl.pid, 12345);
        assert_eq!(rl.comment, "");
    }

    #[test]
    fn from_lockfile_with_comment() {
        let rl = RemoteLock::from_lockfile("1700000000:testuser:12345:my comment").unwrap();
        assert_eq!(rl.comment, "my comment");
    }

    #[test]
    fn from_lockfile_comment_keeps_colons() {
        let rl = RemoteLock::from_lockfile("1700000000:user:999:comment:with:colons").unwrap();
        assert_eq!(rl.user, "user");
        assert_eq!(rl.pid, 999);
        assert_eq!(rl.comment, "comment:with:colons");
    }

    #[test]
    fn from_lockfile_too_few_fields_errors() {
        let err = RemoteLock::from_lockfile("only:one").unwrap_err();
        assert!(matches!(err, HostError::Sftp { reason, .. } if reason.contains("weird format")));
    }

    #[test]
    fn display_with_and_without_comment() {
        let mut rl = RemoteLock {
            user: "testuser".into(),
            ..Default::default()
        };
        assert!(rl.to_string().contains("locked by testuser."));
        rl.comment = "testing".into();
        assert!(rl.to_string().contains("locked by testuser (testing)"));
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let original = RemoteLock {
            timestamp: "1700000000".into(),
            user: "admin".into(),
            pid: 42,
            comment: "test comment".into(),
        };
        let parsed = RemoteLock::from_lockfile(&original.to_lockfile()).unwrap();
        assert_eq!(parsed, original);
    }

    // --- test scaffolding ---------------------------------------------------

    /// A fake clock: fixed `now`, a manually-advanceable monotonic counter, and
    /// a no-op sleep that advances the monotonic clock by the slept amount.
    #[derive(Clone)]
    struct FakeClock {
        now: u64,
        mono: Arc<AtomicU64>, // milliseconds
    }

    impl FakeClock {
        fn new(now: u64) -> Self {
            Self {
                now,
                mono: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl Clock for FakeClock {
        fn now_unix(&self) -> u64 {
            self.now
        }
        fn monotonic(&self) -> f64 {
            self.mono.load(Ordering::SeqCst) as f64 / 1000.0
        }
        async fn sleep(&self, secs: f64) {
            self.mono
                .fetch_add((secs * 1000.0) as u64, Ordering::SeqCst);
        }
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        c.session_user = "testuser".into();
        c
    }

    fn tl(conn: MockConnection, clock: FakeClock) -> TargetLock<FakeClock> {
        TargetLock::with_clock(Box::new(conn), &cfg(), clock)
    }

    fn now() -> u64 {
        1_700_000_000
    }

    // --- TargetLock ---------------------------------------------------------

    #[tokio::test]
    async fn init_uses_config_user_and_current_pid() {
        let lock = tl(MockConnection::new("h1"), FakeClock::new(now()));
        assert_eq!(lock.i_am_user, "testuser");
        assert_eq!(lock.i_am_pid, std::process::id());
    }

    #[tokio::test]
    async fn is_locked_false_when_no_file() {
        let mut lock = tl(MockConnection::new("h1"), FakeClock::new(now()));
        assert!(!lock.is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn is_locked_true_when_foreign_file_present() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(lock.is_locked().await.unwrap());
    }

    #[tokio::test]
    async fn lock_creates_lockfile_via_exclusive_create() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.lock("test comment").await.unwrap();
        // Exactly one exclusive write happened, no overwrite needed.
        let ops = handle.sftp_ops();
        assert_eq!(
            ops,
            vec![crate::connection::MockSftpOp::Write {
                path: PathBuf::from(TARGET_LOCK_PATH),
                exclusive: true,
            }]
        );
        // Written contents are the stamped line.
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:test comment", std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn lock_raises_when_locked_by_other() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        let err = lock.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)));
    }

    #[tokio::test]
    async fn lock_allows_relock_by_same_process() {
        let mine = format!("1700000000:testuser:{}", std::process::id());
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, mine.into_bytes());
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.lock("new comment").await.unwrap(); // must not raise
        // Reconciled with a non-exclusive overwrite carrying the new comment.
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:new comment", std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn try_claim_returns_false_on_fresh_foreign_lock() {
        let mut c = cfg();
        c.lock_wait = 0;
        c.lock_reap_stale = false;
        let fresh = now() - 60;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{fresh}:otheruser:99999").into_bytes(),
        );
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        assert!(!lock.try_claim("mtui pool RRID [me]").await.unwrap());
    }

    #[tokio::test]
    async fn unlock_noop_when_unlocked() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.unlock(false).await.unwrap();
        assert!(
            handle
                .sftp_ops()
                .iter()
                .all(|op| !matches!(op, crate::connection::MockSftpOp::Remove(_)))
        );
    }

    #[tokio::test]
    async fn unlock_removes_own_lock() {
        let mine = format!("1700000000:testuser:{}", std::process::id());
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, mine.into_bytes());
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.unlock(false).await.unwrap();
        assert!(handle.sftp_ops().iter().any(|op| matches!(
            op,
            crate::connection::MockSftpOp::Remove(p) if p == &PathBuf::from(TARGET_LOCK_PATH)
        )));
    }

    #[tokio::test]
    async fn unlock_raises_on_foreign_lock() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        let err = lock.unlock(false).await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)));
    }

    #[tokio::test]
    async fn unlock_force_removes_foreign_lock() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.unlock(true).await.unwrap();
        assert!(
            handle
                .sftp_ops()
                .iter()
                .any(|op| matches!(op, crate::connection::MockSftpOp::Remove(_)))
        );
    }

    #[tokio::test]
    async fn is_mine_variants() {
        let mut lock = tl(MockConnection::new("h1"), FakeClock::new(now()));
        // not locked -> error
        assert!(lock.is_mine().is_err());
        // mine
        lock.lock = RemoteLock {
            user: "testuser".into(),
            pid: std::process::id(),
            ..Default::default()
        };
        assert!(lock.is_mine().unwrap());
        // different user
        lock.lock.user = "otheruser".into();
        assert!(!lock.is_mine().unwrap());
        // same user, different pid
        lock.lock.user = "testuser".into();
        lock.lock.pid = std::process::id() + 1;
        assert!(!lock.is_mine().unwrap());
    }

    #[tokio::test]
    async fn locked_by_msg_contains_host_and_user() {
        let conn = MockConnection::new("host1.example.com").with_file(
            TARGET_LOCK_PATH,
            b"1700000000:testuser:12345:testing".to_vec(),
        );
        let mut lock = tl(conn, FakeClock::new(now()));
        let msg = lock.locked_by_msg().await.unwrap();
        assert!(msg.contains("host1.example.com"));
        assert!(msg.contains("testuser"));
    }

    #[tokio::test]
    async fn time_formats_and_tolerates_bad_timestamp() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:testuser:12345".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(lock.time().await.unwrap().contains("UTC"));

        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"notanumber:otheruser:99999".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        assert_eq!(lock.time().await.unwrap(), "unknown");

        let conn =
            MockConnection::new("h1").with_file(TARGET_LOCK_PATH, b":otheruser:99999".to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        assert_eq!(lock.time().await.unwrap(), "unknown");
    }

    // --- reaping ------------------------------------------------------------

    fn reaping_cfg() -> Config {
        let mut c = cfg();
        c.lock_reap_stale = true;
        c.lock_stale_age = 86400;
        c
    }

    fn reaping_lock(conn: MockConnection) -> TargetLock<FakeClock> {
        TargetLock::with_clock(Box::new(conn), &reaping_cfg(), FakeClock::new(now()))
    }

    #[tokio::test]
    async fn age_seconds_none_when_unlocked() {
        let mut lock = reaping_lock(MockConnection::new("h1"));
        assert_eq!(lock.age_seconds().await.unwrap(), None);
    }

    #[tokio::test]
    async fn age_seconds_none_on_bad_timestamp() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"not-a-number:otheruser:99999".to_vec());
        let mut lock = reaping_lock(conn);
        assert_eq!(lock.age_seconds().await.unwrap(), None);
    }

    #[tokio::test]
    async fn age_seconds_computes_age() {
        let old = now() - 3600;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{old}:otheruser:99999").into_bytes(),
        );
        let mut lock = reaping_lock(conn);
        assert_eq!(lock.age_seconds().await.unwrap(), Some(3600));
    }

    #[tokio::test]
    async fn reap_removes_stale_foreign_lock() {
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999").into_bytes(),
        );
        let handle = conn.clone();
        let mut lock = reaping_lock(conn);
        assert!(lock.reap_if_stale().await.unwrap());
        assert!(
            handle
                .sftp_ops()
                .iter()
                .any(|op| matches!(op, crate::connection::MockSftpOp::Remove(_)))
        );
    }

    #[tokio::test]
    async fn reap_removes_stale_exclusive_lock() {
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999:do not touch").into_bytes(),
        );
        let mut lock = reaping_lock(conn);
        assert!(lock.reap_if_stale().await.unwrap());
    }

    #[tokio::test]
    async fn reap_keeps_fresh_lock() {
        let fresh = now() - 60;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{fresh}:otheruser:99999").into_bytes(),
        );
        let handle = conn.clone();
        let mut lock = reaping_lock(conn);
        assert!(!lock.reap_if_stale().await.unwrap());
        assert!(
            !handle
                .sftp_ops()
                .iter()
                .any(|op| matches!(op, crate::connection::MockSftpOp::Remove(_)))
        );
    }

    #[tokio::test]
    async fn reap_disabled_by_flag() {
        let mut c = reaping_cfg();
        c.lock_reap_stale = false;
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999").into_bytes(),
        );
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        assert!(!lock.reap_if_stale().await.unwrap());
    }

    #[tokio::test]
    async fn reap_disabled_by_zero_age() {
        let mut c = reaping_cfg();
        c.lock_stale_age = 0;
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999").into_bytes(),
        );
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        assert!(!lock.reap_if_stale().await.unwrap());
    }

    #[tokio::test]
    async fn reap_keeps_malformed_lock() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"garbage:otheruser:99999".to_vec());
        let mut lock = reaping_lock(conn);
        assert!(!lock.reap_if_stale().await.unwrap());
    }

    // --- wait queue ---------------------------------------------------------

    #[tokio::test]
    async fn wait_disabled_fails_fast() {
        let mut c = reaping_cfg();
        c.lock_wait = 0;
        let fresh = now() - 60;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{fresh}:otheruser:99999").into_bytes(),
        );
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        let err = lock.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)));
    }

    #[tokio::test]
    async fn wait_succeeds_when_lock_reaped_mid_wait() {
        // wait > 0 and the foreign lock is stale: the first wait_for_lock
        // iteration reaps it and the claim then succeeds.
        let mut c = reaping_cfg();
        c.lock_wait = 5;
        c.lock_wait_poll = 1;
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999").into_bytes(),
        );
        let handle = conn.clone();
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        lock.lock("mtui pool RRID [owner]").await.unwrap(); // must not raise
        // Ends owned by us (a fresh exclusive/overwrite line was written).
        let contents = String::from_utf8(handle.file_contents(TARGET_LOCK_PATH).unwrap()).unwrap();
        assert!(contents.contains("testuser"));
    }

    #[tokio::test]
    async fn wait_times_out_then_raises() {
        let mut c = reaping_cfg();
        c.lock_wait = 1;
        c.lock_wait_poll = 1;
        let fresh = now() - 60;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{fresh}:otheruser:99999").into_bytes(),
        );
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        let err = lock.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)));
    }

    // --- fail-closed reads & race safety ------------------------------------

    #[tokio::test]
    async fn load_fails_closed_on_generic_sftp_error() {
        // A present-but-unreadable lockfile (generic Sftp error, not NotFound)
        // must propagate, not report "unlocked" — otherwise we would claim a
        // host whose true state is unknown.
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec())
            .with_open_error(TARGET_LOCK_PATH);
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(matches!(
            lock.is_locked().await,
            Err(HostError::Sftp { .. })
        ));
    }

    #[tokio::test]
    async fn lock_fails_closed_on_generic_read_error_during_reconcile() {
        // Exclusive create loses (file exists) → reconcile loads the lock, which
        // now errors: the whole lock() must propagate, not overwrite blindly.
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec())
            .with_open_error(TARGET_LOCK_PATH);
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(matches!(lock.lock("").await, Err(HostError::Sftp { .. })));
    }

    #[tokio::test]
    async fn lock_propagates_non_contention_exclusive_create_error() {
        // A non-collision failure of the atomic create (e.g. permission denied)
        // fails closed: it is NOT reconciled as lost contention.
        let conn = MockConnection::new("h1").with_exclusive_write_error(TARGET_LOCK_PATH);
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(matches!(lock.lock("").await, Err(HostError::Sftp { .. })));
    }

    #[tokio::test]
    async fn lock_reconcile_retries_atomic_create_never_overwrites_foreign() {
        // A foreign lock that is freed mid-wait must be re-acquired via the
        // atomic exclusive create, never via a blind non-exclusive overwrite.
        // Model "freed": a stale foreign lock is reaped, then the retried create
        // wins. Assert the only writes are exclusive creates + the final owning
        // line — no non-exclusive overwrite of a foreign line.
        let mut c = reaping_cfg();
        c.lock_wait = 5;
        c.lock_wait_poll = 1;
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            TARGET_LOCK_PATH,
            format!("{stale}:otheruser:99999").into_bytes(),
        );
        let handle = conn.clone();
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        lock.lock("").await.unwrap();
        // Every Write op for the lockfile was exclusive (atomic create); the
        // foreign line was reaped (Remove), never truncated over.
        let writes: Vec<bool> = handle
            .sftp_ops()
            .into_iter()
            .filter_map(|op| match op {
                crate::connection::MockSftpOp::Write { path, exclusive }
                    if path == std::path::Path::new(TARGET_LOCK_PATH) =>
                {
                    Some(exclusive)
                }
                _ => None,
            })
            .collect();
        assert!(
            writes.iter().all(|&excl| excl),
            "reconcile must retry exclusive create, not overwrite: {writes:?}"
        );
        let contents = String::from_utf8(handle.file_contents(TARGET_LOCK_PATH).unwrap()).unwrap();
        assert!(contents.contains("testuser"));
    }

    #[tokio::test]
    async fn unlock_propagates_non_missing_remove_error() {
        // A permission/transport failure on remove must propagate; we must not
        // pretend the lock was released.
        let mine = format!("1700000000:testuser:{}", std::process::id());
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, mine.into_bytes())
            .failing_sftp_remove();
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(matches!(
            lock.unlock(false).await,
            Err(HostError::Sftp { .. })
        ));
        // In-memory state left intact (still ours) since we did not release it.
        assert!(!lock.current().user.is_empty());
    }

    #[tokio::test]
    async fn unlock_ignores_already_missing_lockfile() {
        // An already-gone lockfile (SftpNotFound) counts as released.
        let mine = format!("1700000000:testuser:{}", std::process::id());
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, mine.into_bytes())
            .not_found_sftp_remove();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.unlock(false).await.unwrap();
        assert!(lock.current().user.is_empty());
    }

    // --- PoolLock -----------------------------------------------------------

    fn pool(conn: MockConnection, rrid: &str) -> PoolLock<FakeClock> {
        PoolLock::with_clock(Box::new(conn), &cfg(), rrid, FakeClock::new(now()))
    }

    #[test]
    fn pool_uses_separate_lockfile() {
        assert_ne!(POOL_LOCK_PATH, TARGET_LOCK_PATH);
        assert_eq!(POOL_LOCK_PATH, "/var/lock/mtui-pool.lock");
    }

    #[test]
    fn rrid_of_parses_pool_comment() {
        assert_eq!(
            PoolLock::<SystemClock>::rrid_of("mtui pool SUSE:Maintenance:1:2 [alice]"),
            "SUSE:Maintenance:1:2"
        );
    }

    #[test]
    fn rrid_of_non_pool_comment_empty() {
        assert_eq!(PoolLock::<SystemClock>::rrid_of("testing of something"), "");
        assert_eq!(PoolLock::<SystemClock>::rrid_of(""), "");
    }

    #[test]
    fn pool_is_mine_same_rrid_ignores_pid() {
        let mut p = pool(MockConnection::new("h1"), "SUSE:Maintenance:1:2");
        p.inner.lock = RemoteLock {
            user: "testuser".into(),
            pid: std::process::id() + 1,
            comment: "mtui pool SUSE:Maintenance:1:2 [alice]".into(),
            ..Default::default()
        };
        assert!(p.is_mine().unwrap());
    }

    #[test]
    fn pool_is_mine_different_rrid_not_mine() {
        let mut p = pool(MockConnection::new("h1"), "SUSE:Maintenance:1:2");
        p.inner.lock = RemoteLock {
            user: "testuser".into(),
            pid: std::process::id(),
            comment: "mtui pool SUSE:Maintenance:9:9 [bob]".into(),
            ..Default::default()
        };
        assert!(!p.is_mine().unwrap());
    }

    #[test]
    fn pool_is_mine_different_user_not_mine() {
        let mut p = pool(MockConnection::new("h1"), "SUSE:Maintenance:1:2");
        p.inner.lock = RemoteLock {
            user: "otheruser".into(),
            comment: "mtui pool SUSE:Maintenance:1:2 [alice]".into(),
            ..Default::default()
        };
        assert!(!p.is_mine().unwrap());
    }

    #[test]
    fn pool_is_mine_no_rrid_falls_back_to_user() {
        let mut p = pool(MockConnection::new("h1"), "");
        p.inner.lock = RemoteLock {
            user: "testuser".into(),
            comment: "mtui pool SUSE:Maintenance:9:9 [bob]".into(),
            ..Default::default()
        };
        assert!(p.is_mine().unwrap());
    }

    #[test]
    fn pool_is_mine_raises_when_not_locked() {
        let p = pool(MockConnection::new("h1"), "SUSE:Maintenance:1:2");
        assert!(p.is_mine().is_err());
    }

    #[tokio::test]
    async fn pool_lock_writes_to_pool_lockfile() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        p.lock("mtui pool SUSE:Maintenance:1:2 [alice]")
            .await
            .unwrap();
        // The claim is written to the pool file, never the operation-lock file.
        assert!(handle.file_contents(POOL_LOCK_PATH).is_some());
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_none());
        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            format!(
                "1700000000:testuser:{}:mtui pool SUSE:Maintenance:1:2 [alice]",
                std::process::id()
            )
            .into_bytes()
        );
    }

    #[tokio::test]
    async fn pool_unlock_removes_own_claim() {
        let mine = format!(
            "1700000000:testuser:{}:mtui pool SUSE:Maintenance:1:2 [alice]",
            std::process::id()
        );
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, mine.into_bytes());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        p.unlock(false).await.unwrap();
        assert!(handle.file_contents(POOL_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn pool_unlock_removes_own_claim_from_other_process() {
        // RRID-based ownership: a claim recording our RRID but a *different* PID
        // is still ours (a tester reconnecting from a fresh process).
        let mine = format!(
            "1700000000:testuser:{}:mtui pool SUSE:Maintenance:1:2 [alice]",
            std::process::id() + 1
        );
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, mine.into_bytes());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        p.unlock(false).await.unwrap();
        assert!(handle.file_contents(POOL_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn pool_unlock_refuses_foreign_claim_without_force() {
        let foreign = b"1700000000:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        let err = p.unlock(false).await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)));
        // The foreign claim is left in place.
        assert!(handle.file_contents(POOL_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn pool_unlock_force_removes_foreign_claim() {
        let foreign = b"1700000000:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        p.unlock(true).await.unwrap();
        assert!(handle.file_contents(POOL_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn pool_unlock_noop_when_unclaimed() {
        let mut p = pool(MockConnection::new("h1"), "SUSE:Maintenance:1:2");
        p.unlock(false).await.unwrap(); // must not raise
    }

    #[tokio::test]
    async fn pool_unlock_propagates_non_missing_remove_error() {
        // Fail-closed: a non-gone removal error must propagate and not mark the
        // claim released.
        let mine = format!(
            "1700000000:testuser:{}:mtui pool SUSE:Maintenance:1:2 [alice]",
            std::process::id()
        );
        let conn = MockConnection::new("h1")
            .with_file(POOL_LOCK_PATH, mine.into_bytes())
            .failing_sftp_remove();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(matches!(p.unlock(false).await, Err(HostError::Sftp { .. })));
    }

    #[tokio::test]
    async fn pool_unlock_ignores_already_missing_lockfile() {
        let mine = format!(
            "1700000000:testuser:{}:mtui pool SUSE:Maintenance:1:2 [alice]",
            std::process::id()
        );
        let conn = MockConnection::new("h1")
            .with_file(POOL_LOCK_PATH, mine.into_bytes())
            .not_found_sftp_remove();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        p.unlock(false).await.unwrap(); // already gone → released
    }

    #[tokio::test]
    async fn pool_try_claim_succeeds_on_free_host() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(
            p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap()
        );
        assert!(handle.file_contents(POOL_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn pool_try_claim_returns_false_on_foreign_claim() {
        let foreign = b"1700000000:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]".to_vec();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(
            !p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap()
        );
    }

    /// A pool claim built from `conn`+`rrid` with an explicit `[lock]` `config`
    /// (so reaping knobs can be toggled per test).
    fn pool_with_cfg(conn: MockConnection, rrid: &str, config: &Config) -> PoolLock<FakeClock> {
        PoolLock::with_clock(Box::new(conn), config, rrid, FakeClock::new(now()))
    }

    #[tokio::test]
    async fn pool_try_claim_reaps_stale_foreign_claim() {
        // A foreign pool claim older than pool_stale_age is force-reaped, then
        // re-claimed for us — the only automatic recovery for a claim orphaned by
        // an uncatchable exit (RRID ownership means the dead PID never frees it).
        let stale = now() - 200_000; // >> default pool_stale_age (86400)
        let foreign =
            format!("{stale}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(
            p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap(),
            "a stale foreign claim must be reaped and re-claimed"
        );
        // The reap removed the pool lockfile, and we then wrote our own claim.
        let contents = String::from_utf8(handle.file_contents(POOL_LOCK_PATH).unwrap()).unwrap();
        assert!(contents.contains("SUSE:Maintenance:1:2"));
    }

    #[tokio::test]
    async fn pool_try_claim_does_not_reap_fresh_foreign_claim() {
        // A foreign claim younger than pool_stale_age is left alone and refused.
        let fresh = now() - 10; // well under the 86400 default
        let foreign =
            format!("{fresh}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(
            !p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap(),
            "a fresh foreign claim must not be reaped"
        );
    }

    #[tokio::test]
    async fn pool_try_claim_respects_reaping_disabled() {
        // With pool_reap_stale=false (or pool_stale_age=0) even an ancient foreign
        // claim is left in place and the claim is refused.
        let stale = now() - 200_000;
        let foreign =
            format!("{stale}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes();

        let mut disabled = cfg();
        disabled.pool_reap_stale = false;
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign.clone());
        let mut p = pool_with_cfg(conn, "SUSE:Maintenance:1:2", &disabled);
        assert!(
            !p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap(),
            "pool_reap_stale=false must not reap"
        );

        let mut zero_age = cfg();
        zero_age.pool_stale_age = 0;
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let mut p = pool_with_cfg(conn, "SUSE:Maintenance:1:2", &zero_age);
        assert!(
            !p.try_claim("mtui pool SUSE:Maintenance:1:2 [alice]")
                .await
                .unwrap(),
            "pool_stale_age=0 must disable reaping"
        );
    }

    #[tokio::test]
    async fn pool_reap_if_stale_returns_false_for_fresh_claim() {
        let fresh = now() - 10;
        let foreign =
            format!("{fresh}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes();
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign);
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(!p.reap_if_stale().await.unwrap());
    }

    #[tokio::test]
    async fn pool_set_rrid_updates_ownership_identity() {
        let mut p = pool(MockConnection::new("h1"), "");
        p.set_rrid("SUSE:Maintenance:1:2");
        p.inner.lock = RemoteLock {
            user: "testuser".into(),
            comment: "mtui pool SUSE:Maintenance:1:2 [alice]".into(),
            ..Default::default()
        };
        assert!(p.is_mine().unwrap());
    }
    // --- civil date ---------------------------------------------------------

    #[test]
    fn format_utc_known_timestamp() {
        // 1700000000 = 2023-11-14 22:13:20 UTC, a Tuesday.
        assert_eq!(format_utc(1_700_000_000), "Tuesday, 14.11.2023 22:13 UTC");
    }

    #[test]
    fn format_utc_epoch() {
        assert_eq!(format_utc(0), "Thursday, 01.01.1970 00:00 UTC");
    }
}
