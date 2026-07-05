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

    /// Human-readable "locked by <user> (<comment>)." string.
    #[must_use]
    pub fn describe(&self) -> String {
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
}

/// The default operation-lock path, `/var/lock/mtui.lock` — a cross-mtui
/// contract (a Python and a Rust mtui may share a host fleet).
pub const TARGET_LOCK_PATH: &str = "/var/lock/mtui.lock";

/// The default pool-claim-lock path, `/var/lock/mtui-pool.lock`.
pub const POOL_LOCK_PATH: &str = "/var/lock/mtui-pool.lock";

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
    pub fn with_clock(connection: Box<dyn Connection>, config: &Config, clock: C) -> Self {
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
        }
    }

    /// The lockfile path this lock manages. Overridden by [`PoolLock`].
    fn filename(&self) -> PathBuf {
        PathBuf::from(TARGET_LOCK_PATH)
    }

    /// The remote hostname (for messages).
    fn hostname(&self) -> String {
        self.connection.hostname().to_owned()
    }

    /// Loads the lock state from the remote host into [`Self::lock`].
    ///
    /// A missing lockfile resets to the empty (unlocked) state. Any other SFTP
    /// error propagates.
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
            Err(HostError::SftpNotFound { .. } | HostError::Sftp { .. }) => {
                // Treat a missing/unreadable lockfile as "unlocked", matching
                // upstream's ENOENT branch. A truly absent file surfaces as
                // HostError::SftpNotFound; a present-but-unreadable one as the
                // catch-all HostError::Sftp — both mean "no usable lock".
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
    pub async fn age_seconds(&mut self) -> Result<Option<u64>> {
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
    pub async fn reap_if_stale(&mut self) -> Result<bool> {
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

    /// Locks the target.
    ///
    /// Attempts an atomic exclusive create first; on collision, reconciles
    /// (re-stamp if ours, reap if stale, wait if configured) or refuses.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the host is held by another
    /// owner and cannot be acquired, or an SFTP error from the write path.
    pub async fn lock(&mut self, comment: &str) -> Result<()> {
        let rl = RemoteLock {
            user: self.i_am_user.clone(),
            timestamp: self.clock.now_unix().to_string(),
            pid: self.i_am_pid,
            comment: comment.to_owned(),
        };
        let path = self.filename();

        // 1) Atomic exclusive create: on a free host exactly one racer wins.
        match self
            .connection
            .sftp_write(&path, rl.to_lockfile().as_bytes(), true)
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

        // 2) The file exists. May we overwrite it?
        if self.is_locked().await? && !self.is_mine()? && !self.wait_for_lock().await? {
            return Err(HostError::TargetLocked(self.locked_by_msg().await?));
        }

        self.connection
            .sftp_write(&path, rl.to_lockfile().as_bytes(), false)
            .await?;
        self.lock = rl;
        Ok(())
    }

    /// A "locked by" message suitable for display.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub async fn locked_by_msg(&mut self) -> Result<String> {
        self.load().await?;
        Ok(format!("{} is {}", self.hostname(), self.lock))
    }

    /// The user who currently holds the lock (empty when unlocked).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub async fn locked_by(&mut self) -> Result<String> {
        self.load().await?;
        Ok(self.lock.user.clone())
    }

    /// The comment on the current lock (empty when none/unlocked).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub async fn comment(&mut self) -> Result<String> {
        self.load().await?;
        Ok(self.lock.comment.clone())
    }

    /// A formatted lock-creation time, or `"unknown"` when the stored timestamp
    /// is missing/malformed (must not raise — this is called while reporting a
    /// foreign lock during acquisition).
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub async fn time(&mut self) -> Result<String> {
        self.load().await?;
        match self.lock.timestamp.parse::<i64>() {
            Ok(ts) => Ok(format_utc(ts)),
            Err(_) => Ok("unknown".to_owned()),
        }
    }

    /// Unlocks the target.
    ///
    /// With `force = false` a foreign lock is refused; with `force = true` any
    /// owner's lock is removed. A no-op on an unlocked host.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the lock is foreign and not
    /// forced, or an SFTP error from the remove path.
    pub async fn unlock(&mut self, force: bool) -> Result<()> {
        if !self.is_locked().await? {
            return Ok(());
        }
        if !self.is_mine()? && !force {
            return Err(HostError::TargetLocked(self.locked_by_msg().await?));
        }
        let path = self.filename();
        if let Err(e) = self.connection.sftp_remove(&path).await {
            // A already-gone lockfile is fine; log other failures but do not
            // propagate them (best-effort unlock, mirrors upstream).
            tracing::debug!(host = %self.hostname(), error = %e, "ignoring unlock remove error");
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
    pub fn is_mine(&self) -> Result<bool> {
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
    pub fn current(&self) -> &RemoteLock {
        &self.lock
    }
}

/// Something that can be locked and unlocked for the duration of a scope.
///
/// Implemented by [`TargetLock`] and (later) `Target`, so [`with_locked`] can
/// lock a whole group before a critical section and always unlock it after.
#[async_trait::async_trait]
pub trait Lockable: Send {
    /// Acquire the lock (with an optional comment).
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when held by another owner.
    async fn acquire(&mut self, comment: &str) -> Result<()>;

    /// Release the lock. Best-effort: implementations should not fail the whole
    /// unlock walk on one host's error.
    ///
    /// # Errors
    /// Returns an error only for an unexpected, non-ignorable failure.
    async fn release(&mut self) -> Result<()>;
}

#[async_trait::async_trait]
impl<C: Clock> Lockable for TargetLock<C> {
    async fn acquire(&mut self, comment: &str) -> Result<()> {
        self.lock(comment).await
    }
    async fn release(&mut self) -> Result<()> {
        self.unlock(false).await
    }
}

/// Locks every target, runs `body`, then unlocks every target — even if `body`
/// returns an error or a lock/unlock fails partway.
///
/// This is the async, error-safe equivalent of upstream's `LockedTargets`
/// context manager (`with LockedTargets(targets): ...`). Rust `Drop` cannot be
/// async, so scope semantics are expressed as a passed-in future instead of an
/// RAII guard: acquisition is all-or-nothing (a failure rolls back the locks
/// already taken), and release always runs. The `body` future takes no
/// arguments — the targets are locked *in place* for its duration, exactly like
/// the `with` block, and the caller keeps whatever handles it needs.
///
/// # Errors
///
/// Returns the first error encountered while acquiring, or the error from
/// `body`. Unlock failures during teardown are logged, not propagated (the
/// body's result is what the caller cares about), matching upstream's
/// best-effort `__exit__`.
pub async fn with_locked<T, Fut, R>(targets: &mut [T], comment: &str, body: Fut) -> Result<R>
where
    T: Lockable,
    Fut: std::future::Future<Output = Result<R>>,
{
    // Acquire in order; on failure, roll back what we already took.
    let mut acquire_err = None;
    let mut acquired = 0usize;
    for (i, t) in targets.iter_mut().enumerate() {
        if let Err(e) = t.acquire(comment).await {
            acquire_err = Some(e);
            acquired = i;
            break;
        }
        acquired = i + 1;
    }
    if let Some(e) = acquire_err {
        for taken in targets[..acquired].iter_mut() {
            let _ = taken.release().await;
        }
        return Err(e);
    }

    // Run the body, then always release, regardless of the body's outcome.
    let result = body.await;
    for t in targets.iter_mut() {
        if let Err(e) = t.release().await {
            tracing::debug!(error = %e, "ignoring unlock error during LockedTargets teardown");
        }
    }
    result
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
}

impl PoolLock<SystemClock> {
    /// Builds a `PoolLock` with the real [`SystemClock`].
    #[must_use]
    pub fn new(connection: Box<dyn Connection>, config: &Config, rrid: impl Into<String>) -> Self {
        Self::with_clock(connection, config, rrid, SystemClock)
    }
}

impl<C: Clock> PoolLock<C> {
    /// Builds a `PoolLock` with an explicit [`Clock`] (for tests).
    #[must_use]
    pub fn with_clock(
        connection: Box<dyn Connection>,
        config: &Config,
        rrid: impl Into<String>,
        clock: C,
    ) -> Self {
        Self {
            inner: TargetLock::with_clock(connection, config, clock),
            i_am_rrid: rrid.into(),
        }
    }

    /// Extracts the RRID from a `mtui pool <RRID> [<owner>]` comment, or `""`
    /// when the comment is not a pool comment.
    #[must_use]
    pub fn rrid_of(comment: &str) -> String {
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
    pub fn is_mine(&self) -> Result<bool> {
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
    pub fn filename(&self) -> PathBuf {
        PathBuf::from(POOL_LOCK_PATH)
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

    // --- with_locked (LockedTargets) ---------------------------------------

    #[derive(Default)]
    struct SpyLock {
        locked: Arc<AtomicU64>,
        unlocked: Arc<AtomicU64>,
        fail_acquire: bool,
    }

    #[async_trait::async_trait]
    impl Lockable for SpyLock {
        async fn acquire(&mut self, _comment: &str) -> Result<()> {
            if self.fail_acquire {
                return Err(HostError::TargetLocked("busy".into()));
            }
            self.locked.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn release(&mut self) -> Result<()> {
            self.unlocked.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn spy(counters: &(Arc<AtomicU64>, Arc<AtomicU64>)) -> SpyLock {
        SpyLock {
            locked: counters.0.clone(),
            unlocked: counters.1.clone(),
            fail_acquire: false,
        }
    }

    #[tokio::test]
    async fn with_locked_locks_and_unlocks_all() {
        let c = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let mut targets = vec![spy(&c), spy(&c)];
        let out = with_locked(&mut targets, "", async { Ok(99u32) })
            .await
            .unwrap();
        assert_eq!(out, 99);
        assert_eq!(c.0.load(Ordering::SeqCst), 2, "both locked");
        assert_eq!(c.1.load(Ordering::SeqCst), 2, "both unlocked");
    }

    #[tokio::test]
    async fn with_locked_unlocks_even_when_body_errors() {
        let c = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let mut targets = vec![spy(&c), spy(&c)];
        let res: Result<()> = with_locked(&mut targets, "", async {
            Err(HostError::TargetLocked("boom".into()))
        })
        .await;
        assert!(res.is_err());
        assert_eq!(c.0.load(Ordering::SeqCst), 2, "both locked");
        assert_eq!(
            c.1.load(Ordering::SeqCst),
            2,
            "both unlocked despite body error"
        );
    }

    #[tokio::test]
    async fn with_locked_rolls_back_on_acquire_failure() {
        let c = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let first = spy(&c);
        let failing = SpyLock {
            locked: c.0.clone(),
            unlocked: c.1.clone(),
            fail_acquire: true,
        };
        let mut targets = vec![first, failing];
        let res: Result<()> = with_locked(&mut targets, "", async { Ok(()) }).await;
        assert!(res.is_err());
        // The first target was locked (1) then rolled back (unlocked 1); the
        // failing second never locked. Body never ran.
        assert_eq!(c.0.load(Ordering::SeqCst), 1);
        assert_eq!(c.1.load(Ordering::SeqCst), 1);
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
