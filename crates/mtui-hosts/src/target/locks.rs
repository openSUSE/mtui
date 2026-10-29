//! Remote lock management for target hosts.
//!
//! ## Overview
//!
//! There are **two remote
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
//! is a **cross-process contract**: other tools on the fleet parse the same
//! layout, including older `mtui` releases, so the bytes must not change. The
//! `comment` field keeps any embedded colons (parsed with a 3-way split); a
//! `PoolLock` stores `mtui pool <RRID> [<owner>]` there.
//!
//! A missing file is a free host; a present file that is not such a line
//! (empty, malformed, not UTF-8) is **invalid**, never free: claims refuse it
//! with [`HostError::InvalidLockFile`], a forced unlock removes it, and the
//! stale reap ages it by the file's modification time.
//!
//! ## Atomicity
//!
//! [`TargetLock::lock`] first attempts an **atomic exclusive create**
//! ([`Connection::sftp_write`] with `exclusive = true` →
//! `O_CREAT | O_EXCL`): exactly one of two racing processes wins, and the loser
//! reconciles (re-stamp if it's ours, reap if stale, wait if configured, else
//! refuse). This closes the read-then-write TOCTOU the old "stat then write"
//! sequence had.
//!
//! ## Design choices
//!
//! * All I/O is `async` (the connection layer is async-native).
//! * The wait queue takes an injected [`Clock`] instead of a real timer,
//!   so the polling loop is unit-testable without real sleeps.
//! * A contended lock surfaces as [`HostError::TargetLocked`] rather than a
//!   distinct error type.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
/// `Reporter`/[`HostsGroup`](crate::HostsGroup) lock sinks to
/// the display layer.
///
/// The lock accessors are async `&mut self`; the resolving code does
/// that I/O once and hands these already-resolved (sync) values downstream so
/// the display stays sync and snapshot-testable. Carries the fields the
/// lock listing reads off the lock object.
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
    /// The label of the sibling [`TargetLock`] in this process that holds the
    /// lock, when it is one (see [`OpLockHolders`]). Always `None` for a pool
    /// claim.
    pub sibling: Option<String>,
    /// The lock file exists but is empty or unparseable. It is not free:
    /// `is_locked` is set too, and no owner fields are filled.
    pub invalid: bool,
}

impl LockRow {
    /// The row for a lock file that exists but is empty or unparseable.
    pub(crate) fn invalid() -> Self {
        Self {
            is_locked: true,
            invalid: true,
            ..Self::default()
        }
    }
}

/// A single-read view of a lock's on-disk state plus derived ownership.
///
/// Produced by `TargetLock::snapshot` / `PoolLock::snapshot` with **exactly
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
    /// The sibling in this process that holds the lock, as in
    /// [`LockRow::sibling`].
    pub(crate) sibling: Option<String>,
    /// The lock file exists but is empty or unparseable, as in
    /// [`LockRow::invalid`]; [`lock`](Self::lock) is then the default.
    pub(crate) invalid: bool,
}

/// Who holds a contended lock, for reporting it.
///
/// Read off the load the failed acquisition/release already performed, so
/// naming the owner costs no extra remote round-trip (#521); an empty
/// [`by`](Self::by) means the lock state was never read (unconnected host).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockOwner {
    /// The user recorded in the lockfile.
    pub by: String,
    /// Human-readable lock timestamp, or `"unknown"`.
    pub since: String,
    /// The label of the sibling [`TargetLock`] in this process that holds the
    /// lock, when it is one: another template or MCP session, not another
    /// process (see [`OpLockHolders`]).
    pub sibling: Option<String>,
}

/// How a sibling holder is named to the operator: `label` is the template's
/// RRID, empty for one without a template.
#[must_use]
pub fn sibling_holder_name(label: &str) -> String {
    if label.is_empty() {
        "this mtui process".to_owned()
    } else {
        format!("this mtui process ({label})")
    }
}

/// Which [`TargetLock`] in this process holds each host's operation lock.
///
/// Wire ownership is user + PID, so every `TargetLock` in one process (a second
/// loaded template, a second MCP session) reads another's lock as its own; this
/// registry, shared by those objects, is the only way to tell them apart. Keyed
/// by [`Connection::hostname`], so two names for one host are not recognised as
/// the same. A claim goes away with its `TargetLock`, even when the lockfile
/// stays on the host.
#[derive(Clone, Default)]
pub struct OpLockHolders(Arc<Mutex<HashMap<String, Holder>>>);

struct Holder {
    id: u64,
    label: String,
}

impl OpLockHolders {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Holder>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn claim(&self, host: &str, id: u64, label: &str) {
        self.map().insert(
            host.to_owned(),
            Holder {
                id,
                label: label.to_owned(),
            },
        );
    }

    fn release(&self, host: &str, id: u64) {
        let mut map = self.map();
        if map.get(host).is_some_and(|h| h.id == id) {
            map.remove(host);
        }
    }

    /// The label of the holder of `host` other than `id`, if there is one.
    fn sibling(&self, host: &str, id: u64) -> Option<String> {
        self.map()
            .get(host)
            .filter(|h| h.id != id)
            .map(|h| h.label.clone())
    }

    /// Whether both handles share one registry.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Source of [`TargetLock`] identities; only ever compared for equality.
static NEXT_LOCK_ID: AtomicU64 = AtomicU64::new(1);

/// Which driving surface a contended line renders for.
///
/// Owns the paired `list_locks` steer and `--force` whole-group scope so a
/// call site cannot transpose them: the REPL's plain steer with its `-t`-scoped
/// caveat and the MCP abort path's markdown steer with its per-template fan-out
/// travel as one value. The fan-out truths differ (`-t`-scoped vs per-template
/// whole-group), hence two variants rather than one shared string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContendedSurface {
    /// REPL (`lock`/`run`/`unlock`): plain `list_locks`, `-t`-scoped caveat.
    Repl,
    /// MCP abort path: markdown `list_locks`, per-template whole-group scope.
    Mcp,
}

impl ContendedSurface {
    fn check(self) -> &'static str {
        match self {
            Self::Repl => "check list_locks",
            Self::Mcp => "check with `list_locks`",
        }
    }

    fn scope_note(self) -> &'static str {
        match self {
            Self::Repl => " (unlock --force clears every selected host)",
            Self::Mcp => "; `unlock --force` releases the whole group of every loaded template",
        }
    }
}

/// Names the owner of a contended operation lock and the next safe step.
///
/// Own/foreign/unknown split and hedges shared by the REPL (`lock`/`run`/`unlock`)
/// and the MCP abort path; `surface` selects the paired steer/scope (see
/// [`ContendedSurface`]).
#[must_use]
pub fn contended_lock_reason(
    owner: &LockOwner,
    session_user: &str,
    surface: ContendedSurface,
) -> String {
    let check = surface.check();
    let scope_note = surface.scope_note();
    if let Some(label) = &owner.sibling {
        let holder = sibling_holder_name(label);
        format!("held by {holder}, another template or session; retry once its operation finishes")
    } else if owner.by.is_empty() {
        format!("held by an unknown owner, possibly a live mtui; {check}{scope_note}")
    } else if owner.by == session_user {
        format!(
            "held by {} (you) since {}, possibly another mtui of yours; {check} and your other sessions{scope_note}",
            owner.by, owner.since
        )
    } else {
        format!(
            "held by {} since {}, possibly a live mtui; {check}{scope_note}",
            owner.by, owner.since
        )
    }
}

impl RemoteLock {
    /// Serializes to a lockfile line: `timestamp:user:pid[:comment]`.
    ///
    /// The comment field is only appended when non-empty.
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
    /// The line is split at most 3 times so a comment keeps any embedded
    /// colons. An empty or whitespace-only line, fewer than three fields, a
    /// non-numeric pid or an empty user is not a lock line.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Sftp`] with a "weird format" reason when the line
    /// is not a valid lock line.
    pub fn from_lockfile(line: &str) -> Result<Self> {
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
        if parts[1].is_empty() {
            return Err(HostError::Sftp {
                host: String::new(),
                reason: "got weird format in lockfile: empty user".to_owned(),
            });
        }
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

    /// Human-readable "locked by `<user>` (`<comment>`)." string.
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
    /// Whether the last [`load`](Self::load) found the lockfile present but
    /// empty or unparseable. [`lock`](Self::lock) is then the default, yet the
    /// host is not free.
    invalid: bool,
    /// The comment **this instance** stamped when it took the lock, or `None`
    /// when this instance is not holding it.
    ///
    /// Deliberately *not* derived from the remote line: wire ownership is
    /// per-process (user + PID), so every lock any part of this process took
    /// reads back as "mine" — two loaded templates sharing a refhost, or two
    /// MCP sessions in one server, cannot tell their holds apart that way.
    /// This field is the only record of which `TargetLock` object actually
    /// acquired the lock, and it is what
    /// [`HostsGroup::unlock_held`](crate::HostsGroup::unlock_held) scopes on.
    ///
    /// Set by [`lock`](Self::lock), cleared by [`unlock`](Self::unlock) once
    /// the lockfile is provably gone. [`load`](Self::load) never touches it —
    /// reading a host's lock state is not taking it.
    held: Option<String>,
    /// The remote lockfile this instance manages. Defaults to
    /// [`TARGET_LOCK_PATH`]; [`PoolLock`] builds its inner lock over
    /// [`POOL_LOCK_PATH`] instead, so the shared lock/unlock/load machinery
    /// touches the correct file.
    path: PathBuf,
    /// Where this instance records a hold so siblings in the process can see it.
    holders: OpLockHolders,
    /// This instance's identity in [`holders`](Self::holders).
    id: u64,
    /// How this instance names itself to a sibling that finds it holding the
    /// lock.
    label: String,
}

impl<C: Clock> Drop for TargetLock<C> {
    fn drop(&mut self) {
        self.release();
    }
}

/// The default operation-lock path, `/var/lock/mtui.lock` — a cross-process
/// contract (other tools on the fleet parse the same layout).
pub const TARGET_LOCK_PATH: &str = "/var/lock/mtui.lock";

/// The default pool-claim-lock path, `/var/lock/mtui-pool.lock` — a
/// cross-process contract, like [`TARGET_LOCK_PATH`].
pub const POOL_LOCK_PATH: &str = "/var/lock/mtui-pool.lock";

/// How long an invalid lockfile that cannot be proved older is treated as a
/// claimant mid-write (its create and its write are two SFTP operations)
/// before it is reported invalid.
const INVALID_SETTLE_SECS: u64 = 5;

/// Seconds between re-reads while an invalid lockfile settles.
const INVALID_SETTLE_POLL_SECS: f64 = 1.0;

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
            invalid: false,
            held: None,
            path,
            holders: OpLockHolders::default(),
            id: NEXT_LOCK_ID.fetch_add(1, Ordering::Relaxed),
            label: String::new(),
        }
    }

    /// Joins the registry shared with this lock's siblings, carrying a live
    /// claim across.
    pub(crate) fn set_holders(&mut self, holders: OpLockHolders) {
        if self.holders.ptr_eq(&holders) {
            return;
        }
        let held = self.held.is_some();
        if held {
            self.release();
        }
        self.holders = holders;
        if held {
            self.claim();
        }
    }

    /// Names this instance to a sibling that finds it holding the lock.
    pub(crate) fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
        if self.held.is_some() {
            self.claim();
        }
    }

    fn claim(&self) {
        self.holders
            .claim(self.connection.hostname(), self.id, &self.label);
    }

    fn release(&self) {
        self.holders.release(self.connection.hostname(), self.id);
    }

    fn mark_held(&mut self, comment: String) {
        self.held = Some(comment);
        self.claim();
    }

    /// Drops this instance's in-memory hold record (and its hold-registry
    /// entry). For a hold the remote side proved is gone or another owner's —
    /// a reboot that wiped `/var/lock` and lost the re-take race, or a
    /// lockfile that vanished — so a later `holds()` stops claiming it.
    pub(crate) fn mark_released(&mut self) {
        self.held = None;
        self.release();
    }

    /// The label of the other [`TargetLock`] sharing this registry that holds
    /// the host, if there is one.
    pub(crate) fn sibling_label(&self) -> Option<String> {
        self.holders.sibling(self.connection.hostname(), self.id)
    }

    /// [`sibling_label`](Self::sibling_label), only while the loaded lock is
    /// ours on the wire: a registry entry says nothing about a lock another
    /// user or process has since put there.
    fn loaded_sibling(&self) -> Option<String> {
        if self.is_mine().unwrap_or(false) {
            self.sibling_label()
        } else {
            None
        }
    }

    /// Whether another [`TargetLock`] sharing this registry holds the host.
    fn sibling_holds(&self) -> bool {
        self.sibling_label().is_some()
    }

    /// Whether the loaded lock is this instance's to re-stamp or release: ours
    /// on the wire ([`is_mine`](Self::is_mine)) and not a sibling's hold.
    ///
    /// # Errors
    /// As [`is_mine`](Self::is_mine).
    pub(crate) fn owned_here(&self) -> Result<bool> {
        Ok(self.is_mine()? && !self.sibling_holds())
    }

    /// Whether the last [`load`](Self::load) found an invalid lockfile.
    pub(crate) fn is_invalid(&self) -> bool {
        self.invalid
    }

    /// Whether **this** instance is holding the lock and stamped it with no
    /// comment.
    ///
    /// The two halves answer two different questions:
    ///
    /// * *this instance* — see [`held`](Self::held): PID-based wire ownership
    ///   cannot separate two host groups inside one mtui process.
    /// * *no comment* — a non-empty comment marks an **exclusive** hold
    ///   ([`RemoteLock::comment`]): the PI assignment lock the session
    ///   re-applies on every connect and after every reboot, or an operator's
    ///   `lock <text>` reservation. The operation flows (`update_lock`, `run`'s
    ///   `lock_selected("")`) all stamp an empty comment, so this separates a
    ///   transient operation hold from a deliberate reservation.
    #[must_use]
    pub(crate) fn holds_unmarked(&self) -> bool {
        self.held.as_ref().is_some_and(String::is_empty)
    }

    /// Whether **this** instance is holding the lock, marked or not.
    ///
    /// Unlike [`holds_unmarked`](Self::holds_unmarked), a comment-marked
    /// (exclusive) hold counts too — so `lock -c "reservation"` stays
    /// releasable by a plain `unlock` in the same session, while a sibling
    /// `TargetLock` object (a second loaded template, a second MCP session)
    /// sharing this refhost — whose `held` this instance never set — does not.
    #[must_use]
    pub(crate) fn holds(&self) -> bool {
        self.held.is_some()
    }

    /// The comment this instance stamped when it took the lock, if it holds
    /// one. A reboot wipes `/var/lock` but not this record, so the reboot
    /// lifecycle re-applies exactly this comment.
    #[must_use]
    pub(crate) fn held_comment(&self) -> Option<String> {
        self.held.clone()
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

    /// The error for a lockfile that is present but empty or unparseable,
    /// stating how long ago it was last modified.
    ///
    /// # Errors
    /// Propagates an SFTP error from the modification-time read.
    async fn invalid_error(&mut self) -> Result<HostError> {
        let age = match self.invalid_age().await? {
            Some(secs) => format!("last modified {secs}s ago"),
            None => "last modification time unknown".to_owned(),
        };
        let remedy = if self.path == Path::new(POOL_LOCK_PATH) {
            format!(
                "{age}; if no other mtui is claiming it, `unlock --pool --force` removes \
                 it, or it is reaped once older than `[lock] pool_stale_age`"
            )
        } else {
            format!(
                "{age}; if no other mtui is claiming it, `unlock --force` removes it, \
                 or it is reaped once older than `[lock] stale_age`"
            )
        };
        Ok(HostError::InvalidLockFile {
            host: self.hostname(),
            path: self.path.display().to_string(),
            remedy,
        })
    }

    /// Loads the lock state from the remote host into [`Self::lock`].
    ///
    /// **Fails closed**: only a genuinely *missing* lockfile
    /// ([`HostError::SftpNotFound`]) resets to the empty (unlocked) state.
    /// Every other error — a
    /// permission-denied read, a transport blip, a truncated/garbled read — is
    /// propagated, because reporting "unlocked" for an *unknown* state would let
    /// us claim a host another owner already holds.
    ///
    /// A lockfile that is present but not a valid lock line (empty, not UTF-8,
    /// malformed) sets [`invalid`](Self::invalid) and leaves the lock default:
    /// the host is neither free nor held by a known owner.
    async fn load(&mut self) -> Result<()> {
        self.lock = RemoteLock::default();
        self.invalid = false;
        let path = self.filename();
        match self.connection.sftp_open(&path).await {
            Ok(bytes) => {
                let parsed = std::str::from_utf8(&bytes).ok().and_then(|text| {
                    RemoteLock::from_lockfile(text.lines().next().unwrap_or("")).ok()
                });
                match parsed {
                    Some(lock) => self.lock = lock,
                    None => {
                        tracing::debug!(host = %self.hostname(), path = %path.display(), "invalid lockfile");
                        self.invalid = true;
                    }
                }
                Ok(())
            }
            Err(HostError::SftpNotFound { .. }) => {
                // A truly absent lockfile means "unlocked".
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Whether the host is currently locked (by anyone).
    ///
    /// # Errors
    /// Propagates an SFTP error from `load`.
    pub async fn is_locked(&mut self) -> Result<bool> {
        self.load().await?;
        Ok(self.invalid || !self.lock.user.is_empty())
    }

    /// [`is_locked`](Self::is_locked) for a caller about to claim: an invalid
    /// lockfile is given the chance to settle first.
    async fn is_locked_settled(&mut self) -> Result<bool> {
        self.load().await?;
        self.settle().await?;
        Ok(self.invalid || !self.lock.user.is_empty())
    }

    /// The time since an invalid lockfile was last modified, or `None` when the
    /// server reports no modification time or one in the future.
    async fn invalid_age(&mut self) -> Result<Option<u64>> {
        match self.connection.sftp_mtime(&self.path).await {
            Ok(Some(mtime)) => Ok(self.clock.now_unix().checked_sub(mtime)),
            Ok(None) | Err(HostError::SftpNotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Re-reads a just-loaded invalid lockfile that may still be being written.
    ///
    /// A claimant creates the file and writes its line in two steps, so a file
    /// that reads empty may belong to a live claimant. One provably older than
    /// [`INVALID_SETTLE_SECS`] is invalid at once; otherwise the file is re-read
    /// every [`INVALID_SETTLE_POLL_SECS`] until it parses, vanishes, or the
    /// window is spent. On return [`invalid`](Self::invalid) is true only if it
    /// is still invalid.
    async fn settle(&mut self) -> Result<()> {
        if !self.invalid {
            return Ok(());
        }
        if self
            .invalid_age()
            .await?
            .is_some_and(|age| age >= INVALID_SETTLE_SECS)
        {
            return Ok(());
        }
        let deadline = self.clock.monotonic() + INVALID_SETTLE_SECS as f64;
        while self.invalid {
            let remaining = deadline - self.clock.monotonic();
            if remaining <= 0.0 {
                break;
            }
            self.clock
                .sleep(INVALID_SETTLE_POLL_SECS.min(remaining))
                .await;
            self.load().await?;
        }
        Ok(())
    }

    /// The age of the current lock in seconds, or `None` when unlocked or the
    /// stored timestamp is missing/malformed (callers treat such a lock as
    /// "leave it alone").
    ///
    /// An invalid lockfile has no stored timestamp, so its age is the time since
    /// the remote file was last modified; `None` when the server reports no
    /// modification time, or one in the future.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load) or the mtime read.
    async fn age_seconds(&mut self) -> Result<Option<u64>> {
        self.load().await?;
        if self.invalid {
            return self.invalid_age().await;
        }
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
        if self.invalid {
            tracing::warn!(
                host = %self.hostname(), hours = age / 3600,
                "removing stale invalid lockfile"
            );
            return self.remove_invalid().await;
        }
        tracing::warn!(
            host = %self.hostname(), user = %self.lock.user, hours = age / 3600,
            "removing stale lock"
        );
        self.unlock(true).await?;
        Ok(true)
    }

    /// Removes the lockfile only if it is still invalid on a fresh read, so a
    /// claimant that finished writing its line in the meantime keeps it.
    ///
    /// Returns whether the file was removed (an already-missing one counts).
    ///
    /// # Errors
    /// Propagates an SFTP error from the read or the remove.
    async fn remove_invalid(&mut self) -> Result<bool> {
        self.load().await?;
        if !self.invalid {
            return Ok(false);
        }
        let path = self.filename();
        match self.connection.sftp_remove(&path).await {
            Ok(()) | Err(HostError::SftpNotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        self.invalid = false;
        self.mark_released();
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
        if self.is_locked_settled().await? && !self.owned_here()? && !self.reap_if_stale().await? {
            return if self.invalid {
                Err(self.invalid_error().await?)
            } else {
                Ok(false)
            };
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
            if !self.is_locked().await? || self.owned_here()? {
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
    /// *already owns*. Ownership on the wire is PID-based, so "already owns"
    /// alone cannot separate this instance from a sibling `TargetLock` in the
    /// same process (a second loaded template, a second MCP session) that
    /// took a deliberate, comment-marked reservation under our identical
    /// identity: the re-stamp additionally refuses when this instance never
    /// took *this* lock (`held.is_none()`) and the loaded comment is a
    /// reservation that differs from the one about to be written — a
    /// same-comment re-apply (the PI autolock after a reconnect) still
    /// succeeds. When wait/reap frees the host, the acquisition is a
    /// **retried atomic exclusive create**, so a racer that took the lock in
    /// the TOCTOU window between our last check and the write wins and we
    /// reconcile again rather than stomping their line.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the host is held by another
    /// owner and cannot be acquired, when a sibling `TargetLock`'s deliberate
    /// reservation would otherwise be overwritten, or an SFTP error from the
    /// read/write path.
    pub async fn lock(&mut self, comment: &str) -> Result<()> {
        let rl = RemoteLock {
            user: self.i_am_user.clone(),
            timestamp: self.clock.now_unix().to_string(),
            pid: self.i_am_pid,
            comment: comment.to_owned(),
        };
        let path = self.filename();
        let line = rl.to_lockfile();
        // Reconciles a create whose reply timed out at most once per `lock()`
        // call — the create may well have landed server-side, so the next
        // load is trusted as authoritative instead of assuming failure. A
        // *second* timeout is a real, propagating error: this is not another
        // `RECONCILE_RETRIES`-style budget.
        let mut timeout_reconciled = false;

        for _ in 0..Self::RECONCILE_RETRIES {
            // 1) Atomic exclusive create: on a free host exactly one racer wins.
            match self
                .connection
                .sftp_write(&path, line.as_bytes(), true)
                .await
            {
                Ok(()) => {
                    self.mark_held(rl.comment.clone());
                    self.lock = rl;
                    return Ok(());
                }
                Err(HostError::AlreadyExists { .. }) => {
                    // Fall through to reconciliation.
                }
                Err(HostError::SftpTimeout { .. }) if !timeout_reconciled => {
                    timeout_reconciled = true;
                    // The create may well have landed server-side; re-read
                    // before giving up.
                }
                Err(e) => return Err(e),
            }

            // 2) The file exists. Load it (fail-closed) and decide.
            self.load().await?;
            self.settle().await?;
            if self.lock.user.is_empty() && !self.invalid {
                // Freed between the create and the load — retry the create.
                continue;
            }
            if self.owned_here()? {
                // The wire's ownership check is PID-based, so it cannot tell
                // this instance apart from a sibling `TargetLock` in the same
                // process (a second loaded template, a second MCP session)
                // that stamped a deliberate reservation under our identical
                // identity. Refuse the re-stamp rather than clobber it: this
                // instance never took *this* lock (`held.is_none()`), the
                // loaded comment is non-empty (a reservation — an operation
                // lock stamps `""`, so this never blocks a plain re-lock), and
                // it differs from the comment we are about to write (a
                // same-comment re-apply, e.g. the PI autolock after a
                // reconnect, is not a clobber).
                if self.held.is_none()
                    && !self.lock.comment.is_empty()
                    && self.lock.comment != rl.comment
                {
                    return Err(HostError::TargetLocked(self.locked_by_msg().await?));
                }
                // An operation re-lock over our own reservation changes
                // nothing: rewriting it would strip the comment.
                if rl.comment.is_empty()
                    && self.held.as_ref().is_some_and(|c| !c.is_empty())
                    && self.held.as_deref() == Some(self.lock.comment.as_str())
                {
                    return Ok(());
                }
                if timeout_reconciled {
                    // The line on the host is the one our timed-out create
                    // just wrote: adopt it as-is rather than risking a second
                    // timeout on a redundant re-stamp. The loaded `self.lock`
                    // is authoritative.
                    self.mark_held(self.lock.comment.clone());
                    return Ok(());
                }
                // Legitimate re-stamp of our own lock (possibly a new comment).
                self.connection
                    .sftp_write(&path, line.as_bytes(), false)
                    .await?;
                self.mark_held(rl.comment.clone());
                self.lock = rl;
                return Ok(());
            }
            // Foreign lock: reap-if-stale / wait may free it, then we retry the
            // atomic create (never a blind overwrite of a foreign line).
            if self.wait_for_lock().await? {
                continue;
            }
            return Err(self.contention().await?);
        }

        // Exhausted retries: treat as contended and fail closed.
        Err(self.contention().await?)
    }

    /// The error for a lock that cannot be taken: [`HostError::InvalidLockFile`]
    /// when the lockfile is invalid, else [`HostError::TargetLocked`] naming the
    /// holder.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    async fn contention(&mut self) -> Result<HostError> {
        let msg = self.locked_by_msg().await?;
        if self.invalid {
            return self.invalid_error().await;
        }
        Ok(HostError::TargetLocked(msg))
    }

    /// A "locked by" message suitable for display.
    ///
    /// # Errors
    /// Propagates an SFTP error from [`load`](Self::load).
    pub(crate) async fn locked_by_msg(&mut self) -> Result<String> {
        self.load().await?;
        if self.invalid {
            return Ok(self.invalid_error().await?.to_string());
        }
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
    /// ([`HostError::SftpNotFound`]) counts as released, but
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
            // The lockfile is gone (a reboot cleared `/var/lock`, another owner
            // removed it): whatever this instance took, it no longer holds.
            self.mark_released();
            return Ok(());
        }
        if !self.owned_here()? && !force {
            return Err(self.contention().await?);
        }
        let path = self.filename();
        match self.connection.sftp_remove(&path).await {
            Ok(()) => {}
            Err(HostError::SftpNotFound { .. }) => {
                // Already gone — treat as released.
                tracing::debug!(host = %self.hostname(), "lockfile already gone");
            }
            Err(e) => {
                // A permission/transport failure did NOT remove the lock; do not
                // pretend we released it (fail closed — leave self.lock and the
                // `held` record intact).
                return Err(e);
            }
        }
        self.lock = RemoteLock::default();
        self.invalid = false;
        self.mark_released();
        Ok(())
    }

    /// Whether the currently-loaded lock is owned by this process.
    ///
    /// PID-based: mine only when both the user and the PID match. Call after a
    /// method that loads the lock (e.g. [`is_locked`](Self::is_locked)).
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] with a "not locked" reason when no
    /// lock is loaded.
    pub(crate) fn is_mine(&self) -> Result<bool> {
        if self.invalid {
            return Ok(false);
        }
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

    /// The owner from the most recent [`load`](Self::load), with **no** remote
    /// read.
    ///
    /// Never stale, only ever empty: [`load`](Self::load) resets the cache
    /// before reading, so an unread lock yields blank fields rather than a
    /// previous owner. The invariant callers must maintain is that the
    /// [`HostError::TargetLocked`] they report came off a path that loaded — the
    /// acquire/release paths do, via
    /// [`locked_by_msg`](Self::locked_by_msg), whereas
    /// [`is_mine`](Self::is_mine)'s "not locked" error raises with no load and
    /// stays out of these reports only because every caller guards it with
    /// [`is_locked`](Self::is_locked) first. Re-reading per field instead would
    /// put two more SFTP round-trips on the contended path (#482, #521).
    #[must_use]
    pub(crate) fn loaded_owner(&self) -> LockOwner {
        LockOwner {
            by: self.lock.user.clone(),
            since: self.lock.display_time(),
            sibling: self.loaded_sibling(),
        }
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
            sibling: self.loaded_sibling(),
            invalid: self.invalid,
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
        if self.inner.invalid {
            return Ok(false);
        }
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
            sibling: None,
            invalid: self.inner.invalid,
        })
    }

    /// The pool-claim comment this instance stamped, if it holds the claim.
    /// See [`TargetLock::held_comment`](Self::held_comment).
    #[must_use]
    pub(crate) fn held_comment(&self) -> Option<String> {
        self.inner.held_comment()
    }

    /// Drops this instance's in-memory claim record after the remote claim
    /// proved to be another template's, so `holds()` stops claiming it.
    pub(crate) fn mark_released(&mut self) {
        self.inner.mark_released();
    }

    /// [`TargetLock::loaded_owner`] for the pool claim — no remote read.
    #[must_use]
    pub(crate) fn loaded_owner(&self) -> LockOwner {
        self.inner.loaded_owner()
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
    /// Creates the pool lockfile atomically. A collision is reconciled by
    /// **RRID** ownership, never the operation lock's PID check: a claim that is
    /// already ours ([`is_mine`](Self::is_mine)) is adopted byte-for-byte —
    /// without a truncating re-stamp, and without refreshing its timestamp — a
    /// stale foreign claim is reaped and the create retried, and a fresh foreign
    /// claim is refused untouched. Pool acquisition never waits.
    ///
    /// # Errors
    /// Returns [`HostError::TargetLocked`] when the host is claimed by another
    /// owner, or an SFTP error from the read/write path.
    async fn lock(&mut self, comment: &str) -> Result<()> {
        let rl = RemoteLock {
            user: self.inner.i_am_user.clone(),
            timestamp: self.inner.clock.now_unix().to_string(),
            pid: self.inner.i_am_pid,
            comment: comment.to_owned(),
        };
        let path = self.filename();
        let line = rl.to_lockfile();
        // A timed-out create may have landed, so the next load is trusted once;
        // a second timeout is a real error.
        let mut timeout_reconciled = false;

        for _ in 0..TargetLock::<C>::RECONCILE_RETRIES {
            match self
                .inner
                .connection
                .sftp_write(&path, line.as_bytes(), true)
                .await
            {
                Ok(()) => {
                    self.inner.mark_held(rl.comment.clone());
                    self.inner.lock = rl;
                    return Ok(());
                }
                Err(HostError::AlreadyExists { .. }) => {}
                Err(HostError::SftpTimeout { .. }) if !timeout_reconciled => {
                    timeout_reconciled = true;
                }
                Err(e) => return Err(e),
            }

            self.inner.load().await?;
            self.inner.settle().await?;
            if self.inner.lock.user.is_empty() && !self.inner.invalid {
                continue;
            }
            if self.is_mine()? {
                self.inner.mark_held(self.inner.lock.comment.clone());
                return Ok(());
            }
            if self.reap_if_stale().await? {
                continue;
            }
            return Err(self.inner.contention().await?);
        }

        Err(self.inner.contention().await?)
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
        if self.inner.invalid {
            tracing::warn!(
                host = %self.inner.hostname(),
                hours = age / 3600,
                "removing stale invalid pool lockfile"
            );
            return self.inner.remove_invalid().await;
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
        if self.inner.is_locked_settled().await?
            && !self.is_mine()?
            && !self.reap_if_stale().await?
        {
            return if self.inner.invalid {
                Err(self.inner.invalid_error().await?)
            } else {
                Ok(false)
            };
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
            return Err(self.inner.contention().await?);
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
        self.inner.invalid = false;
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

    /// Own/foreign/unknown arms stay distinguishable both ways, hedge, and keep
    /// `--force`'s whole-group scope visible instead of "force this one host".
    #[test]
    fn contended_lock_reason_hedges_and_scopes_the_force_remedy() {
        let alice = LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
            ..Default::default()
        };
        let foreign = contended_lock_reason(&alice, "bob", ContendedSurface::Repl);
        let mine = contended_lock_reason(&alice, "alice", ContendedSurface::Repl);
        let unknown = contended_lock_reason(&LockOwner::default(), "bob", ContendedSurface::Repl);

        for line in [&foreign, &mine, &unknown] {
            assert!(line.contains("list_locks"), "{line}");
            assert!(
                line.contains("unlock --force clears every selected host"),
                "{line}"
            );
            assert!(line.contains("possibly"), "hedge missing: {line}");
            assert!(
                !line.contains('`') && !line.contains("every loaded template"),
                "REPL line carries the MCP steer/scope: {line}"
            );
        }
        assert!(
            foreign.contains("held by alice since Tuesday, 14.11.2023 22:13 UTC"),
            "{foreign}"
        );
        assert!(
            foreign.contains("possibly a live mtui")
                && !foreign.contains("(you)")
                && !foreign.contains("mtui of yours"),
            "{foreign}"
        );
        assert!(
            mine.contains("held by alice (you) since Tuesday, 14.11.2023 22:13 UTC")
                && mine.contains("possibly another mtui of yours")
                && mine.contains("check list_locks and your other sessions"),
            "{mine}"
        );
        assert!(!mine.contains("possibly a live mtui"), "{mine}");
        assert!(
            unknown.contains("held by an unknown owner") && !unknown.contains("override"),
            "{unknown}"
        );
    }

    /// The MCP markdown steer renders through the same split, keeping its
    /// per-template whole-group scope (#544).
    #[test]
    fn contended_lock_reason_renders_the_markdown_scope() {
        let alice = LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
            ..Default::default()
        };
        let foreign = contended_lock_reason(&alice, "bob", ContendedSurface::Mcp);
        let mine = contended_lock_reason(&alice, "alice", ContendedSurface::Mcp);
        let unknown = contended_lock_reason(&LockOwner::default(), "bob", ContendedSurface::Mcp);

        for line in [&foreign, &mine, &unknown] {
            assert!(line.contains("`list_locks`"), "{line}");
            assert!(
                line.contains("`unlock --force` releases the whole group of every loaded template"),
                "{line}"
            );
            assert!(
                !line.contains("clears every selected host"),
                "MCP line carries the REPL scope: {line}"
            );
        }
        assert!(
            foreign.contains("held by alice since Tuesday, 14.11.2023 22:13 UTC")
                && !foreign.contains("(you)"),
            "{foreign}"
        );
        assert!(
            mine.contains("held by alice (you) since Tuesday, 14.11.2023 22:13 UTC")
                && mine.contains("possibly another mtui of yours"),
            "{mine}"
        );
        assert!(unknown.contains("held by an unknown owner"), "{unknown}");
    }

    /// The steer/scope pair travels with the surface variant, so REPL vs MCP
    /// cannot be transposed at a call site: the same owner renders each
    /// surface's own steer and scope, never the other's.
    #[test]
    fn contended_surfaces_keep_their_own_steer_and_scope() {
        let alice = LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
            ..Default::default()
        };
        let repl = contended_lock_reason(&alice, "bob", ContendedSurface::Repl);
        let mcp = contended_lock_reason(&alice, "bob", ContendedSurface::Mcp);

        assert!(repl.contains("check list_locks"), "{repl}");
        assert!(
            repl.contains("(unlock --force clears every selected host)"),
            "{repl}"
        );
        assert!(
            !repl.contains('`') && !repl.contains("every loaded template"),
            "{repl}"
        );
        assert!(mcp.contains("check with `list_locks`"), "{mcp}");
        assert!(
            mcp.contains("`unlock --force` releases the whole group of every loaded template"),
            "{mcp}"
        );
        assert!(!mcp.contains("clears every selected host"), "{mcp}");
    }

    #[test]
    fn contended_lock_reason_names_a_sibling_and_offers_no_force() {
        let sibling = LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
            sibling: Some("SUSE:Maintenance:1:1".to_owned()),
        };
        let unlabelled = LockOwner {
            sibling: Some(String::new()),
            ..sibling.clone()
        };
        for surface in [ContendedSurface::Repl, ContendedSurface::Mcp] {
            let named = contended_lock_reason(&sibling, "alice", surface);
            assert!(
                named.contains("this mtui process (SUSE:Maintenance:1:1)"),
                "{named}"
            );
            let bare = contended_lock_reason(&unlabelled, "alice", surface);
            assert!(
                bare.contains("this mtui process,") && !bare.contains("()"),
                "{bare}"
            );
            for line in [&named, &bare] {
                assert!(
                    !line.contains("--force") && !line.contains("possibly another mtui"),
                    "{line}"
                );
            }
        }
    }

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
    async fn lock_refuses_to_overwrite_a_sibling_reservation_with_a_different_comment() {
        // Two `TargetLock` instances share one connection and the same
        // process identity (`tl()`'s shared `cfg()` user, the real PID) — as
        // if two loaded templates attached the same refhost. `lock_a` takes a
        // deliberate reservation; `lock_b` — a fresh instance that never took
        // it (`held == None`) — must not clobber it with a different comment,
        // even though the wire's PID-based `is_mine` reads `lock_b` as the
        // owner too.
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut lock_a = tl(conn.clone(), FakeClock::new(now()));
        lock_a.lock("reservation A").await.unwrap();

        let mut lock_b = tl(conn, FakeClock::new(now()));
        let err = lock_b.lock("reservation B").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)), "{err}");
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:reservation A", std::process::id()).into_bytes(),
            "A's reservation must survive byte-identical"
        );
    }

    #[tokio::test]
    async fn lock_reapplies_the_same_comment_after_a_reconnect_without_refusing() {
        // A reconnect rebuilds `TargetLock` from scratch, clearing `held` —
        // but the PI autolock re-applies the *same* comment on every
        // connect/reboot, and that must still succeed: the loaded comment
        // matches what is about to be written, so this is not a clobber.
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut first = tl(conn.clone(), FakeClock::new(now()));
        first.lock("PI assignment").await.unwrap();

        let mut reconnected = tl(conn, FakeClock::new(now()));
        reconnected.lock("PI assignment").await.unwrap();
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:PI assignment", std::process::id()).into_bytes()
        );
    }

    fn tl_shared(
        conn: MockConnection,
        clock: FakeClock,
        holders: &OpLockHolders,
    ) -> TargetLock<FakeClock> {
        let mut lock = tl(conn, clock);
        lock.set_holders(holders.clone());
        lock
    }

    #[tokio::test]
    async fn lock_refuses_a_sibling_operation_lock() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();

        let err = b.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)), "{err}");
        assert!(!b.holds(), "the refused sibling must not record a hold");
        assert!(a.holds());
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn unlock_refuses_a_sibling_hold() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();

        let err = b.unlock(false).await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)), "{err}");
        assert!(
            handle.file_contents(TARGET_LOCK_PATH).is_some(),
            "a sibling's lockfile must survive"
        );
        assert!(a.holds());
    }

    #[tokio::test]
    async fn force_unlock_still_removes_a_sibling_hold() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();

        b.unlock(true).await.unwrap();
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn wait_treats_a_sibling_as_busy() {
        let mut c = cfg();
        c.lock_wait = 3;
        c.lock_wait_poll = 1;
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let clock = FakeClock::new(now());
        let mut b = TargetLock::with_clock(Box::new(conn), &c, clock.clone());
        b.set_holders(holders);
        a.lock("").await.unwrap();

        let err = b.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)), "{err}");
        assert_eq!(
            clock.monotonic(),
            3.0,
            "the queue must run out the whole budget once, not re-enter per retry"
        );
    }

    #[tokio::test]
    async fn dropping_the_holder_frees_the_host() {
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();
        drop(a);

        b.lock("").await.unwrap();
        assert!(b.holds());
    }

    #[tokio::test]
    async fn a_refused_sibling_reports_who_holds_the_host() {
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.set_label("SUSE:Maintenance:1:1");
        a.lock("").await.unwrap();

        b.lock("").await.unwrap_err();
        assert_eq!(
            b.loaded_owner().sibling.as_deref(),
            Some("SUSE:Maintenance:1:1")
        );
        let snap = b.snapshot().await.unwrap();
        assert_eq!(snap.sibling.as_deref(), Some("SUSE:Maintenance:1:1"));
    }

    #[tokio::test]
    async fn a_label_set_after_locking_reaches_the_registry() {
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();
        a.set_label("SUSE:Maintenance:1:1");

        b.lock("").await.unwrap_err();
        assert_eq!(
            b.loaded_owner().sibling.as_deref(),
            Some("SUSE:Maintenance:1:1")
        );
    }

    #[tokio::test]
    async fn the_holder_itself_and_a_foreign_lock_name_no_sibling() {
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();
        assert_eq!(a.snapshot().await.unwrap().sibling, None);
        assert_eq!(a.loaded_owner().sibling, None);

        let foreign = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec());
        let mut b = tl_shared(foreign, FakeClock::new(now()), &holders);
        b.lock("").await.unwrap_err();
        assert_eq!(
            b.loaded_owner().sibling,
            None,
            "a lock of another user is not the sibling's, whatever the registry says"
        );
    }

    #[tokio::test]
    async fn lock_keeps_an_own_reservation_byte_for_byte() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.lock("X").await.unwrap();
        let reserved = handle.file_contents(TARGET_LOCK_PATH).unwrap();

        lock.lock("").await.unwrap();
        assert_eq!(handle.file_contents(TARGET_LOCK_PATH).unwrap(), reserved);
        assert!(!lock.holds_unmarked(), "it is still a reservation");
        assert!(lock.holds());
    }

    #[tokio::test]
    async fn lock_with_a_comment_still_restamps_an_own_reservation() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.lock("X").await.unwrap();

        lock.lock("Y").await.unwrap();
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:Y", std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn unlock_frees_the_host_for_a_sibling() {
        let conn = MockConnection::new("h1");
        let holders = OpLockHolders::default();
        let mut a = tl_shared(conn.clone(), FakeClock::new(now()), &holders);
        let mut b = tl_shared(conn, FakeClock::new(now()), &holders);
        a.lock("").await.unwrap();
        a.unlock(false).await.unwrap();
        assert!(
            holders.map().is_empty(),
            "a released hold must be deregistered"
        );

        b.lock("").await.unwrap();
        assert!(b.holds());
    }

    #[tokio::test]
    async fn set_holders_moves_a_live_claim_to_the_new_registry() {
        let conn = MockConnection::new("h1");
        let mut a = tl(conn.clone(), FakeClock::new(now()));
        a.lock("").await.unwrap();

        let shared = OpLockHolders::default();
        a.set_holders(shared.clone());
        let mut b = tl_shared(conn, FakeClock::new(now()), &shared);
        let err = b.lock("").await.unwrap_err();
        assert!(matches!(err, HostError::TargetLocked(_)), "{err}");
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
    async fn lock_adopts_a_lock_whose_create_timed_out() {
        // The create's reply timed out, but the mock's fixture landed our own
        // stamped line server-side (the free path was inserted before the
        // timeout was raised). `lock()` must reconcile this as a success,
        // adopting the loaded (our own) line rather than treating the host as
        // unowned.
        let conn = MockConnection::new("h1").with_exclusive_write_timeout(TARGET_LOCK_PATH);
        let handle = conn.clone();
        let mut lock = tl(conn, FakeClock::new(now()));
        lock.lock("test comment")
            .await
            .expect("adopts the landed lock instead of erroring");
        assert!(lock.is_mine().unwrap());
        // Exactly one exclusive write (the timed-out create) and the read that
        // reconciled it — no redundant re-stamp that could time out again.
        let ops = handle.sftp_ops();
        assert_eq!(
            ops,
            vec![
                crate::connection::MockSftpOp::Write {
                    path: PathBuf::from(TARGET_LOCK_PATH),
                    exclusive: true,
                },
                crate::connection::MockSftpOp::Open(PathBuf::from(TARGET_LOCK_PATH)),
            ]
        );
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            format!("1700000000:testuser:{}:test comment", std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn lock_does_not_adopt_a_foreign_lock_after_a_timeout() {
        // The path was already held by a foreign owner before our create's
        // reply timed out; the fixture's exclusive-create semantics leave that
        // foreign line untouched. Fail-closed: never adopted.
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242".to_vec())
            .with_exclusive_write_timeout(TARGET_LOCK_PATH);
        let mut lock = tl(conn, FakeClock::new(now()));
        let err = lock
            .lock("")
            .await
            .expect_err("a foreign lock must never be adopted");
        assert!(matches!(err, HostError::TargetLocked(msg) if msg.contains("alice")));
    }

    #[tokio::test]
    async fn lock_propagates_a_second_timeout() {
        // The timeout-reconciliation pass is one-shot. A very stale foreign
        // lock is present, so the first timed-out create's reconciliation load
        // sees it, reaps it (freeing the retried create's path), and the
        // *retried* create also times out — this second timeout must
        // propagate rather than reconciling again.
        let mut c = reaping_cfg();
        c.lock_wait = 5;
        c.lock_wait_poll = 1;
        let stale = now() - 200_000; // >> reaping_cfg's lock_stale_age (86400)
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, format!("{stale}:alice:4242").into_bytes())
            .with_exclusive_write_timeout(TARGET_LOCK_PATH);
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
        let err = lock
            .lock("")
            .await
            .expect_err("a second timeout propagates");
        assert!(matches!(err, HostError::SftpTimeout { .. }));
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

    const POOL_COMMENT: &str = "mtui pool SUSE:Maintenance:1:2 [new owner]";

    fn pool_line(user: &str, pid: u32, rrid: &str) -> Vec<u8> {
        format!("{}:{user}:{pid}:mtui pool {rrid} [previous owner]", now()).into_bytes()
    }

    fn pool_writes(handle: &MockConnection) -> Vec<(PathBuf, bool)> {
        handle
            .sftp_ops()
            .into_iter()
            .filter_map(|op| match op {
                crate::connection::MockSftpOp::Write { path, exclusive } => Some((path, exclusive)),
                _ => None,
            })
            .collect()
    }

    fn pool_removes(handle: &MockConnection) -> usize {
        handle
            .sftp_ops()
            .into_iter()
            .filter(|op| matches!(op, crate::connection::MockSftpOp::Remove(_)))
            .count()
    }

    #[tokio::test]
    async fn pool_reacquire_same_rrid_other_pid() {
        let other_pid = std::process::id() + 1;
        assert_ne!(other_pid, std::process::id());
        let original = pool_line("testuser", other_pid, "SUSE:Maintenance:1:2");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(p.try_claim(POOL_COMMENT).await.unwrap());

        assert!(p.snapshot().await.unwrap().is_mine);
        assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), original);
        assert_eq!(pool_removes(&handle), 0);
        assert!(
            pool_writes(&handle).iter().all(|(_, excl)| *excl),
            "adoption must not rewrite the claim"
        );
    }

    /// Asserts a refused/adopted claim left `original` untouched with no
    /// removal and no truncating write.
    fn assert_pool_line_untouched(handle: &MockConnection, original: &[u8]) {
        assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), original);
        assert_eq!(pool_removes(handle), 0);
        assert!(pool_writes(handle).iter().all(|(_, excl)| *excl));
    }

    #[tokio::test]
    async fn pool_reacquire_same_pid_adopts_identical_bytes() {
        let original = pool_line("testuser", std::process::id(), "SUSE:Maintenance:1:2");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(p.try_claim(POOL_COMMENT).await.unwrap());

        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn pool_reacquire_refuses_other_rrid_even_with_same_pid() {
        let original = pool_line("testuser", std::process::id(), "SUSE:Maintenance:9:9");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(!p.try_claim(POOL_COMMENT).await.unwrap());

        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn pool_reacquire_refuses_other_user_with_same_rrid() {
        let original = pool_line("otheruser", std::process::id(), "SUSE:Maintenance:1:2");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(!p.try_claim(POOL_COMMENT).await.unwrap());

        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn pool_reacquire_without_session_rrid_matches_on_user_alone() {
        let original = pool_line("testuser", std::process::id() + 1, "SUSE:Maintenance:9:9");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "");

        assert!(p.try_claim(POOL_COMMENT).await.unwrap());

        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn pool_reacquire_free_host_is_an_exclusive_create_with_the_stamp() {
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(p.try_claim(POOL_COMMENT).await.unwrap());

        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            format!("{}:testuser:{}:{POOL_COMMENT}", now(), std::process::id()).into_bytes()
        );
        assert_eq!(
            pool_writes(&handle),
            vec![(PathBuf::from(POOL_LOCK_PATH), true)]
        );
    }

    #[tokio::test]
    async fn pool_reacquire_reaps_a_stale_foreign_claim_then_creates_exclusively() {
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1").with_file(
            POOL_LOCK_PATH,
            format!("{stale}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes(),
        );
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        p.lock(POOL_COMMENT).await.unwrap();

        assert_eq!(pool_removes(&handle), 1);
        assert!(pool_writes(&handle).iter().all(|(_, excl)| *excl));
        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            format!("{}:testuser:{}:{POOL_COMMENT}", now(), std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn pool_reacquire_does_not_reap_a_fresh_claim_for_having_a_dead_pid() {
        let original = pool_line("testuser", 4_000_000_000, "SUSE:Maintenance:9:9");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(matches!(
            p.lock(POOL_COMMENT).await,
            Err(HostError::TargetLocked(_))
        ));

        assert_pool_line_untouched(&handle, &original);
    }

    /// Forwards to a [`MockConnection`], planting `foreign` right before the
    /// first exclusive write and logging every write/remove it forwards.
    /// `on_mtime` is planted the same way, right before the first mtime read.
    type LandOnOpen = (u32, Option<Vec<u8>>);

    #[derive(Clone)]
    struct RacingConnection {
        inner: MockConnection,
        foreign: Arc<Mutex<Option<Vec<u8>>>>,
        on_mtime: Arc<Mutex<Option<Vec<u8>>>>,
        /// `(n, line)`: the first `n` opens see the file as it is, the next one
        /// finds `line` written (or, for `None`, the file removed), as a
        /// claimant's WRITE landing after its OPEN.
        land_on_open: Arc<Mutex<Option<LandOnOpen>>>,
        forwarded_writes: Arc<Mutex<Vec<bool>>>,
        forwarded_removes: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl Connection for RacingConnection {
        fn hostname(&self) -> &str {
            self.inner.hostname()
        }
        fn clone_box(&self) -> Box<dyn Connection> {
            Box::new(self.clone())
        }
        async fn run(&mut self, command: &str) -> Result<mtui_types::hostlog::CommandLog> {
            self.inner.run(command).await
        }
        fn is_active(&self) -> bool {
            self.inner.is_active()
        }
        fn set_command_timeout(&mut self, timeout: crate::connection::CommandTimeout) {
            self.inner.set_command_timeout(timeout)
        }
        async fn close(&mut self) -> Result<()> {
            self.inner.close().await
        }
        async fn reconnect(&mut self, retry: usize, backoff: bool) -> Result<()> {
            self.inner.reconnect(retry, backoff).await
        }
        async fn fire_and_forget(&mut self, command: &str) -> Result<()> {
            self.inner.fire_and_forget(command).await
        }
        async fn sftp_put(
            &mut self,
            local: &std::path::Path,
            remote: &std::path::Path,
        ) -> Result<()> {
            self.inner.sftp_put(local, remote).await
        }
        async fn sftp_put_bytes(&mut self, data: &[u8], remote: &std::path::Path) -> Result<()> {
            self.inner.sftp_put_bytes(data, remote).await
        }
        async fn sftp_get(
            &mut self,
            remote: &std::path::Path,
            local: &std::path::Path,
        ) -> Result<()> {
            self.inner.sftp_get(remote, local).await
        }
        async fn sftp_get_folder(
            &mut self,
            remote: &std::path::Path,
            local: &std::path::Path,
        ) -> Result<()> {
            self.inner.sftp_get_folder(remote, local).await
        }
        async fn sftp_listdir(&mut self, path: &std::path::Path) -> Result<Vec<String>> {
            self.inner.sftp_listdir(path).await
        }
        async fn sftp_open(&mut self, path: &std::path::Path) -> Result<Vec<u8>> {
            let landed = {
                let mut pending = self.land_on_open.lock().unwrap();
                match pending.as_mut() {
                    Some((n, _)) if *n > 0 => {
                        *n -= 1;
                        None
                    }
                    Some(_) => pending.take().map(|(_, line)| line),
                    None => None,
                }
            };
            if let Some(line) = landed {
                match line {
                    Some(line) => self.inner.clone().sftp_write(path, &line, false).await?,
                    None => self.inner.clone().sftp_remove(path).await?,
                }
            }
            self.inner.sftp_open(path).await
        }
        async fn sftp_read_capped(
            &mut self,
            path: &std::path::Path,
            max_bytes: usize,
        ) -> Result<crate::connection::SftpRead> {
            self.inner.sftp_read_capped(path, max_bytes).await
        }
        async fn sftp_write(
            &mut self,
            path: &std::path::Path,
            data: &[u8],
            exclusive: bool,
        ) -> Result<()> {
            let planted = if exclusive {
                self.foreign.lock().unwrap().take()
            } else {
                None
            };
            if let Some(foreign) = planted {
                self.inner.clone().sftp_write(path, &foreign, false).await?;
            }
            self.forwarded_writes.lock().unwrap().push(exclusive);
            self.inner.sftp_write(path, data, exclusive).await
        }
        async fn sftp_append(&mut self, path: &std::path::Path, data: &[u8]) -> Result<()> {
            self.inner.sftp_append(path, data).await
        }
        async fn sftp_remove(&mut self, path: &std::path::Path) -> Result<()> {
            *self.forwarded_removes.lock().unwrap() += 1;
            self.inner.sftp_remove(path).await
        }
        async fn sftp_mtime(&mut self, path: &std::path::Path) -> Result<Option<u64>> {
            let planted = self.on_mtime.lock().unwrap().take();
            if let Some(content) = planted {
                self.inner.clone().sftp_write(path, &content, false).await?;
            }
            self.inner.sftp_mtime(path).await
        }
        async fn sftp_rmdir(&mut self, path: &std::path::Path) -> Result<()> {
            self.inner.sftp_rmdir(path).await
        }
        async fn sftp_readlink(&mut self, path: &std::path::Path) -> Result<String> {
            self.inner.sftp_readlink(path).await
        }
        async fn sftp_session(&mut self) -> Result<Box<dyn crate::connection::SftpSession + '_>> {
            self.inner.sftp_session().await
        }
        #[cfg(feature = "shell")]
        async fn shell(
            &mut self,
            cols: u32,
            rows: u32,
        ) -> Result<Box<dyn crate::connection::ShellChannel>> {
            self.inner.shell(cols, rows).await
        }
    }

    #[tokio::test]
    async fn pool_reacquire_never_overwrites_a_claim_that_appears_after_the_precheck() {
        let foreign = pool_line("otheruser", 99, "SUSE:Maintenance:9:9");
        let inner = MockConnection::new("h1");
        let handle = inner.clone();
        let racing = RacingConnection {
            inner,
            foreign: Arc::new(Mutex::new(Some(foreign.clone()))),
            on_mtime: Arc::default(),
            land_on_open: Arc::default(),
            forwarded_writes: Arc::default(),
            forwarded_removes: Arc::default(),
        };
        let writes = racing.forwarded_writes.clone();
        let removes = racing.forwarded_removes.clone();
        let mut p = PoolLock::with_clock(
            Box::new(racing),
            &cfg(),
            "SUSE:Maintenance:1:2",
            FakeClock::new(now()),
        );

        assert!(!p.try_claim(POOL_COMMENT).await.unwrap());

        assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), foreign);
        assert_eq!(*writes.lock().unwrap(), vec![true]);
        assert_eq!(*removes.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn pool_reacquire_adopts_a_create_whose_reply_timed_out_exactly_once() {
        let conn = MockConnection::new("h1").with_exclusive_write_timeout(POOL_LOCK_PATH);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        p.lock(POOL_COMMENT).await.unwrap();

        assert!(p.snapshot().await.unwrap().is_mine);
        assert_eq!(
            handle.sftp_ops()[..2],
            [
                crate::connection::MockSftpOp::Write {
                    path: PathBuf::from(POOL_LOCK_PATH),
                    exclusive: true,
                },
                crate::connection::MockSftpOp::Open(PathBuf::from(POOL_LOCK_PATH)),
            ]
        );
        assert_eq!(pool_writes(&handle).len(), 1);
        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            format!("{}:testuser:{}:{POOL_COMMENT}", now(), std::process::id()).into_bytes()
        );
    }

    #[tokio::test]
    async fn pool_reacquire_does_not_adopt_a_foreign_claim_after_a_timeout() {
        let original = pool_line("otheruser", 99, "SUSE:Maintenance:9:9");
        let conn = MockConnection::new("h1")
            .with_file(POOL_LOCK_PATH, original.clone())
            .with_exclusive_write_timeout(POOL_LOCK_PATH);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(matches!(
            p.lock(POOL_COMMENT).await,
            Err(HostError::TargetLocked(msg)) if msg.contains("otheruser")
        ));

        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn pool_reacquire_propagates_an_unreadable_claim() {
        let conn = MockConnection::new("h1")
            .with_file(
                POOL_LOCK_PATH,
                pool_line("testuser", 1, "SUSE:Maintenance:1:2"),
            )
            .with_open_error(POOL_LOCK_PATH);
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(matches!(
            p.lock(POOL_COMMENT).await,
            Err(HostError::Sftp { .. })
        ));
    }

    #[tokio::test]
    async fn pool_reacquire_propagates_a_second_create_timeout() {
        let stale = now() - 200_000;
        let conn = MockConnection::new("h1")
            .with_file(
                POOL_LOCK_PATH,
                format!("{stale}:otheruser:99:mtui pool SUSE:Maintenance:9:9 [bob]").into_bytes(),
            )
            .with_exclusive_write_timeout(POOL_LOCK_PATH);
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(matches!(
            p.lock(POOL_COMMENT).await,
            Err(HostError::SftpTimeout { .. })
        ));
    }
    // --- invalid lock files -------------------------------------------------

    /// Content that must never read as free: empty, whitespace-only, wrong
    /// field count, a non-numeric pid, no user, not UTF-8.
    const INVALID_CONTENTS: [&[u8]; 9] = [
        b"",
        b"  \n\t\n",
        b"\n",
        b"just some text",
        b"1700000000:alice",
        b"1700000000:alice:notapid",
        b"1700000000::4242",
        b"\xff\xfe\xfd",
        b"1700000000:alice:4242:note-\xe9",
    ];

    const VALID_FOREIGN: &[u8] = b"1700000000:otheruser:4242:busy";

    fn creates(handle: &MockConnection, path: &str) -> usize {
        handle
            .sftp_ops()
            .into_iter()
            .filter(|op| {
                matches!(op, crate::connection::MockSftpOp::Write { path: p, exclusive: true }
                    if p == std::path::Path::new(path))
            })
            .count()
    }

    fn removes(handle: &MockConnection, path: &str) -> usize {
        handle
            .sftp_ops()
            .into_iter()
            .filter(|op| {
                matches!(op, crate::connection::MockSftpOp::Remove(p)
                    if p == std::path::Path::new(path))
            })
            .count()
    }

    fn assert_invalid(err: HostError, path: &str) {
        match err {
            HostError::InvalidLockFile {
                host,
                path: p,
                remedy,
            } => {
                assert_eq!(host, "h1");
                assert_eq!(p, path);
                assert!(remedy.contains("--force"), "{remedy}");
            }
            other => panic!("expected InvalidLockFile, got {other:?}"),
        }
    }

    /// `(remote mtime, whether the stale policy reaps)` against the default
    /// 24 h stale age at [`now`].
    fn mtime_cases() -> [(Option<u64>, bool); 5] {
        [
            (Some(now() - 86_401), true),
            (Some(now() - 86_400), false),
            (Some(now() - 60), false),
            (Some(now() + 60), false),
            (None, false),
        ]
    }

    fn with_mtime(conn: MockConnection, path: &str, mtime: Option<u64>) -> MockConnection {
        match mtime {
            Some(m) => conn.with_mtime(path, m),
            None => conn,
        }
    }

    #[test]
    fn from_lockfile_rejects_what_is_not_a_lock_line() {
        for bad in [
            "",
            "   ",
            "1700000000:alice",
            "1:alice:x",
            "1700000000::4242",
        ] {
            assert!(RemoteLock::from_lockfile(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn an_invalid_op_lock_is_locked_and_reported_invalid() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec());
            let mut lock = tl(conn, FakeClock::new(now()));
            assert!(lock.is_locked().await.unwrap(), "{content:?}");
            let snap = lock.snapshot().await.unwrap();
            assert!(snap.invalid, "{content:?}");
            assert!(!snap.is_mine, "{content:?}");
            assert!(snap.lock.user.is_empty(), "{content:?}");
        }
    }

    #[tokio::test]
    async fn a_missing_or_valid_op_lock_is_not_invalid() {
        let mut lock = tl(MockConnection::new("h1"), FakeClock::new(now()));
        assert!(!lock.is_locked().await.unwrap());
        assert!(!lock.snapshot().await.unwrap().invalid);
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, VALID_FOREIGN.to_vec());
        let mut lock = tl(conn, FakeClock::new(now()));
        assert!(lock.is_locked().await.unwrap());
        assert!(!lock.snapshot().await.unwrap().invalid);
    }

    #[tokio::test]
    async fn an_invalid_op_lock_refuses_a_claim_after_one_create() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut lock = tl(conn, FakeClock::new(now()));

            let err = lock.lock("").await.unwrap_err();

            assert_invalid(err, TARGET_LOCK_PATH);
            assert_eq!(creates(&handle, TARGET_LOCK_PATH), 1, "{content:?}");
            assert_eq!(handle.file_contents(TARGET_LOCK_PATH).unwrap(), content);
            assert_eq!(removes(&handle, TARGET_LOCK_PATH), 0);
            assert!(!lock.holds());
        }
    }

    #[tokio::test]
    async fn an_invalid_op_lock_fails_try_claim_without_writing() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut lock = reaping_lock(conn);

            let err = lock.try_claim("").await.unwrap_err();

            assert_invalid(err, TARGET_LOCK_PATH);
            assert_eq!(creates(&handle, TARGET_LOCK_PATH), 0, "{content:?}");
            assert_eq!(handle.file_contents(TARGET_LOCK_PATH).unwrap(), content);
        }
    }

    #[tokio::test]
    async fn a_valid_foreign_op_lock_still_reads_as_contended_not_invalid() {
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, VALID_FOREIGN.to_vec());
        let handle = conn.clone();
        let mut lock = reaping_lock(conn);
        assert!(matches!(
            lock.lock("").await,
            Err(HostError::TargetLocked(_))
        ));
        assert!(!lock.try_claim("").await.unwrap());
        assert!(matches!(
            lock.unlock(false).await,
            Err(HostError::TargetLocked(_))
        ));
        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            VALID_FOREIGN
        );
    }

    #[tokio::test]
    async fn a_plain_unlock_leaves_an_invalid_op_lock_alone() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut lock = tl(conn, FakeClock::new(now()));

            let err = lock.unlock(false).await.unwrap_err();

            assert_invalid(err, TARGET_LOCK_PATH);
            assert_eq!(handle.file_contents(TARGET_LOCK_PATH).unwrap(), content);
            assert_eq!(removes(&handle, TARGET_LOCK_PATH), 0);
        }
    }

    #[tokio::test]
    async fn a_forced_unlock_removes_an_invalid_op_lock_and_the_host_can_then_be_claimed() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut lock = tl(conn, FakeClock::new(now()));

            lock.unlock(true).await.unwrap();

            assert!(
                handle.file_contents(TARGET_LOCK_PATH).is_none(),
                "{content:?}"
            );
            assert!(!lock.is_invalid());
            assert!(!lock.is_locked().await.unwrap());
            lock.lock("").await.unwrap();
            assert!(lock.holds());
        }
    }

    #[tokio::test]
    async fn an_old_invalid_op_lock_is_reaped_by_its_modification_time() {
        for content in INVALID_CONTENTS {
            for (mtime, reaped) in mtime_cases() {
                let conn = with_mtime(
                    MockConnection::new("h1").with_file(TARGET_LOCK_PATH, content.to_vec()),
                    TARGET_LOCK_PATH,
                    mtime,
                );
                let handle = conn.clone();
                let mut lock = reaping_lock(conn);

                assert_eq!(
                    lock.reap_if_stale().await.unwrap(),
                    reaped,
                    "{content:?} mtime {mtime:?}"
                );

                assert_eq!(
                    handle.file_contents(TARGET_LOCK_PATH).is_none(),
                    reaped,
                    "{content:?} mtime {mtime:?}"
                );
                assert_eq!(lock.is_invalid(), !reaped, "{content:?} mtime {mtime:?}");
            }
        }
    }

    #[tokio::test]
    async fn the_age_of_an_invalid_lock_is_its_mtime_age_and_unknown_without_one() {
        for (mtime, age) in [
            (Some(now() - 3600), Some(3600)),
            (Some(now()), Some(0)),
            (Some(now() + 60), None),
            (None, None),
        ] {
            let conn = with_mtime(
                MockConnection::new("h1").with_file(TARGET_LOCK_PATH, Vec::new()),
                TARGET_LOCK_PATH,
                mtime,
            );
            let mut lock = reaping_lock(conn);
            assert_eq!(lock.age_seconds().await.unwrap(), age, "mtime {mtime:?}");
        }
    }

    #[tokio::test]
    async fn the_stale_policy_switches_still_gate_an_invalid_op_lock() {
        let old = Some(now() - 200_000);
        let mut off = reaping_cfg();
        off.lock_reap_stale = false;
        let mut zero = reaping_cfg();
        zero.lock_stale_age = 0;
        for c in [off, zero] {
            let conn = with_mtime(
                MockConnection::new("h1").with_file(TARGET_LOCK_PATH, Vec::new()),
                TARGET_LOCK_PATH,
                old,
            );
            let handle = conn.clone();
            let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));
            assert!(!lock.reap_if_stale().await.unwrap());
            assert!(handle.file_contents(TARGET_LOCK_PATH).is_some());
        }
    }

    #[tokio::test]
    async fn a_valid_op_lock_is_never_reaped_by_its_modification_time() {
        let fresh = now() - 60;
        let conn = MockConnection::new("h1")
            .with_file(
                TARGET_LOCK_PATH,
                format!("{fresh}:otheruser:99999").into_bytes(),
            )
            .with_mtime(TARGET_LOCK_PATH, now() - 200_000);
        let handle = conn.clone();
        let mut lock = reaping_lock(conn);
        assert!(!lock.reap_if_stale().await.unwrap());
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn a_failed_remove_of_a_stale_invalid_lock_propagates_and_keeps_it_invalid() {
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, Vec::new())
            .with_file(POOL_LOCK_PATH, Vec::new())
            .with_mtime(TARGET_LOCK_PATH, now() - 200_000)
            .with_mtime(POOL_LOCK_PATH, now() - 200_000)
            .failing_sftp_remove();
        let mut lock = reaping_lock(conn.clone());
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(matches!(
            lock.reap_if_stale().await,
            Err(HostError::Sftp { .. })
        ));
        assert!(matches!(
            p.reap_if_stale().await,
            Err(HostError::Sftp { .. })
        ));
        assert!(lock.is_invalid());
        assert!(p.inner.invalid);
    }

    #[tokio::test]
    async fn the_op_reap_keeps_a_lock_that_became_valid_before_the_remove() {
        let racing = RacingConnection {
            inner: MockConnection::new("h1")
                .with_file(TARGET_LOCK_PATH, Vec::new())
                .with_mtime(TARGET_LOCK_PATH, now() - 200_000),
            foreign: Arc::default(),
            on_mtime: Arc::new(Mutex::new(Some(VALID_FOREIGN.to_vec()))),
            land_on_open: Arc::default(),
            forwarded_writes: Arc::default(),
            forwarded_removes: Arc::default(),
        };
        let handle = racing.inner.clone();
        let removes = racing.forwarded_removes.clone();
        let mut lock =
            TargetLock::with_clock(Box::new(racing), &reaping_cfg(), FakeClock::new(now()));

        assert!(!lock.reap_if_stale().await.unwrap());

        assert_eq!(
            handle.file_contents(TARGET_LOCK_PATH).unwrap(),
            VALID_FOREIGN
        );
        assert_eq!(*removes.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_waiting_claim_reaps_an_old_invalid_op_lock_and_takes_the_host() {
        let mut c = reaping_cfg();
        c.lock_wait = 5;
        c.lock_wait_poll = 1;
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, Vec::new())
            .with_mtime(TARGET_LOCK_PATH, now() - 200_000);
        let handle = conn.clone();
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));

        lock.lock("").await.unwrap();

        assert!(lock.holds());
        assert!(
            String::from_utf8(handle.file_contents(TARGET_LOCK_PATH).unwrap())
                .unwrap()
                .contains("testuser")
        );
    }

    #[tokio::test]
    async fn a_waiting_claim_gives_up_on_a_young_invalid_op_lock_with_the_invalid_error() {
        let mut c = reaping_cfg();
        c.lock_wait = 2;
        c.lock_wait_poll = 1;
        let conn = MockConnection::new("h1")
            .with_file(TARGET_LOCK_PATH, Vec::new())
            .with_mtime(TARGET_LOCK_PATH, now() - 60);
        let handle = conn.clone();
        let mut lock = TargetLock::with_clock(Box::new(conn), &c, FakeClock::new(now()));

        let err = lock.lock("").await.unwrap_err();

        assert_invalid(err, TARGET_LOCK_PATH);
        assert_eq!(creates(&handle, TARGET_LOCK_PATH), 1);
        assert!(handle.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn the_invalid_error_names_the_host_the_path_and_the_way_out() {
        for (path, flag, key) in [
            (TARGET_LOCK_PATH, "unlock --force", "stale_age"),
            (POOL_LOCK_PATH, "unlock --pool --force", "pool_stale_age"),
        ] {
            let mut lock = TargetLock::with_clock_and_path(
                Box::new(
                    MockConnection::new("h1")
                        .with_file(path, Vec::new())
                        .with_mtime(path, now() - 90),
                ),
                &cfg(),
                FakeClock::new(now()),
                PathBuf::from(path),
            );
            let msg = lock.invalid_error().await.unwrap().to_string();
            assert!(msg.contains("h1") && msg.contains(path), "{msg}");
            assert!(msg.contains("invalid (empty or unparseable)"), "{msg}");
            assert!(msg.contains(flag) && msg.contains(key), "{msg}");
            assert!(msg.contains("last modified 90s ago"), "{msg}");
            assert!(msg.contains("if no other mtui is claiming it"), "{msg}");
        }
    }

    // --- invalid pool claims ------------------------------------------------

    #[tokio::test]
    async fn an_invalid_pool_claim_is_locked_and_reported_invalid() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec());
            let mut p = pool(conn, "SUSE:Maintenance:1:2");
            assert!(p.is_locked().await.unwrap(), "{content:?}");
            let snap = p.snapshot().await.unwrap();
            assert!(snap.invalid, "{content:?}");
            assert!(!snap.is_mine, "{content:?}");
        }
    }

    #[tokio::test]
    async fn an_invalid_pool_claim_refuses_a_claim_after_one_create() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut p = pool(conn, "SUSE:Maintenance:1:2");

            let err = p.lock(POOL_COMMENT).await.unwrap_err();

            assert_invalid(err, POOL_LOCK_PATH);
            assert_eq!(creates(&handle, POOL_LOCK_PATH), 1, "{content:?}");
            assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), content);
            assert_eq!(removes(&handle, POOL_LOCK_PATH), 0);
        }
    }

    #[tokio::test]
    async fn an_invalid_pool_claim_fails_try_claim_without_writing() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut p = pool(conn, "SUSE:Maintenance:1:2");

            let err = p.try_claim(POOL_COMMENT).await.unwrap_err();

            assert_invalid(err, POOL_LOCK_PATH);
            assert_eq!(creates(&handle, POOL_LOCK_PATH), 0, "{content:?}");
            assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), content);
        }
    }

    #[tokio::test]
    async fn a_valid_foreign_pool_claim_is_still_just_busy() {
        let original = pool_line("otheruser", 99, "SUSE:Maintenance:9:9");
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, original.clone());
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");
        assert!(!p.try_claim(POOL_COMMENT).await.unwrap());
        assert!(matches!(
            p.unlock(false).await,
            Err(HostError::TargetLocked(_))
        ));
        assert_pool_line_untouched(&handle, &original);
    }

    #[tokio::test]
    async fn a_plain_pool_unlock_leaves_an_invalid_claim_alone() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut p = pool(conn, "SUSE:Maintenance:1:2");

            let err = p.unlock(false).await.unwrap_err();

            assert_invalid(err, POOL_LOCK_PATH);
            assert_eq!(handle.file_contents(POOL_LOCK_PATH).unwrap(), content);
            assert_eq!(removes(&handle, POOL_LOCK_PATH), 0);
        }
    }

    #[tokio::test]
    async fn a_forced_pool_unlock_removes_an_invalid_claim_and_the_host_can_then_be_claimed() {
        for content in INVALID_CONTENTS {
            let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec());
            let handle = conn.clone();
            let mut p = pool(conn, "SUSE:Maintenance:1:2");

            p.unlock(true).await.unwrap();

            assert!(
                handle.file_contents(POOL_LOCK_PATH).is_none(),
                "{content:?}"
            );
            assert!(!p.inner.invalid);
            assert!(p.try_claim(POOL_COMMENT).await.unwrap());
        }
    }

    #[tokio::test]
    async fn an_old_invalid_pool_claim_is_reaped_by_its_modification_time() {
        for content in INVALID_CONTENTS {
            for (mtime, reaped) in mtime_cases() {
                let conn = with_mtime(
                    MockConnection::new("h1").with_file(POOL_LOCK_PATH, content.to_vec()),
                    POOL_LOCK_PATH,
                    mtime,
                );
                let handle = conn.clone();
                let mut p = pool(conn, "SUSE:Maintenance:1:2");

                assert_eq!(
                    p.reap_if_stale().await.unwrap(),
                    reaped,
                    "{content:?} mtime {mtime:?}"
                );

                assert_eq!(
                    handle.file_contents(POOL_LOCK_PATH).is_none(),
                    reaped,
                    "{content:?} mtime {mtime:?}"
                );
                assert_eq!(p.inner.invalid, !reaped, "{content:?} mtime {mtime:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_pool_claim_attempt_reaps_an_old_invalid_claim_and_takes_the_host() {
        let conn = MockConnection::new("h1")
            .with_file(POOL_LOCK_PATH, Vec::new())
            .with_mtime(POOL_LOCK_PATH, now() - 200_000);
        let handle = conn.clone();
        let mut p = pool(conn, "SUSE:Maintenance:1:2");

        assert!(p.try_claim(POOL_COMMENT).await.unwrap());

        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            format!("{}:testuser:{}:{POOL_COMMENT}", now(), std::process::id()).into_bytes()
        );
        assert_eq!(removes(&handle, POOL_LOCK_PATH), 1);
    }

    #[tokio::test]
    async fn the_pool_stale_switches_still_gate_an_invalid_claim() {
        let mut off = cfg();
        off.pool_reap_stale = false;
        let mut zero = cfg();
        zero.pool_stale_age = 0;
        for c in [off, zero] {
            let conn = MockConnection::new("h1")
                .with_file(POOL_LOCK_PATH, Vec::new())
                .with_mtime(POOL_LOCK_PATH, now() - 200_000);
            let handle = conn.clone();
            let mut p = PoolLock::with_clock(
                Box::new(conn),
                &c,
                "SUSE:Maintenance:1:2",
                FakeClock::new(now()),
            );
            assert!(!p.reap_if_stale().await.unwrap());
            assert!(handle.file_contents(POOL_LOCK_PATH).is_some());
        }
    }

    #[tokio::test]
    async fn the_pool_reap_keeps_a_claim_that_became_valid_before_the_remove() {
        let racing = RacingConnection {
            inner: MockConnection::new("h1")
                .with_file(POOL_LOCK_PATH, Vec::new())
                .with_mtime(POOL_LOCK_PATH, now() - 200_000),
            foreign: Arc::default(),
            on_mtime: Arc::new(Mutex::new(Some(pool_line(
                "otheruser",
                99,
                "SUSE:Maintenance:9:9",
            )))),
            land_on_open: Arc::default(),
            forwarded_writes: Arc::default(),
            forwarded_removes: Arc::default(),
        };
        let handle = racing.inner.clone();
        let removes = racing.forwarded_removes.clone();
        let mut p = PoolLock::with_clock(
            Box::new(racing),
            &cfg(),
            "SUSE:Maintenance:1:2",
            FakeClock::new(now()),
        );

        assert!(!p.reap_if_stale().await.unwrap());

        assert_eq!(
            handle.file_contents(POOL_LOCK_PATH).unwrap(),
            pool_line("otheruser", 99, "SUSE:Maintenance:9:9")
        );
        assert_eq!(*removes.lock().unwrap(), 0);
    }

    // --- a claimant between its create and its write --------------------------

    /// How the settle tests seed the file's modification time.
    #[derive(Clone, Copy, Debug)]
    enum Age {
        /// Seconds before [`now`].
        Ago(u64),
        Future,
        Unknown,
    }

    /// Op-lock or pool-claim side of a settle scenario.
    #[derive(Clone, Copy, Debug)]
    enum Side {
        OpLock,
        OpTryClaim,
        PoolLock,
        PoolTryClaim,
    }

    const SIDES: [Side; 4] = [
        Side::OpLock,
        Side::OpTryClaim,
        Side::PoolLock,
        Side::PoolTryClaim,
    ];

    impl Side {
        fn path(self) -> &'static str {
            match self {
                Self::OpLock | Self::OpTryClaim => TARGET_LOCK_PATH,
                Self::PoolLock | Self::PoolTryClaim => POOL_LOCK_PATH,
            }
        }

        fn valid_line(self) -> Vec<u8> {
            match self {
                Self::OpLock | Self::OpTryClaim => VALID_FOREIGN.to_vec(),
                Self::PoolLock | Self::PoolTryClaim => {
                    pool_line("otheruser", 99, "SUSE:Maintenance:9:9")
                }
            }
        }
    }

    /// Runs bob's claim on `side` against an empty file seeded with `age`, whose
    /// owner's line lands after `lands_after` reads (`None`: it never does).
    /// Returns bob's outcome (`Ok(claimed)`), the fake clock's elapsed seconds,
    /// and a handle on the remote file.
    async fn claim_against_empty_file(
        side: Side,
        age: Age,
        lands_after: Option<u32>,
    ) -> (Result<bool>, f64, MockConnection) {
        let path = side.path();
        let mut inner = MockConnection::new("h1").with_file(path, Vec::new());
        match age {
            Age::Ago(s) => inner = inner.with_mtime(path, now() - s),
            Age::Future => inner = inner.with_mtime(path, now() + 60),
            Age::Unknown => {}
        }
        let handle = inner.clone();
        let racing = RacingConnection {
            inner,
            foreign: Arc::default(),
            on_mtime: Arc::default(),
            land_on_open: Arc::new(Mutex::new(
                lands_after.map(|n| (n, Some(side.valid_line()))),
            )),
            forwarded_writes: Arc::default(),
            forwarded_removes: Arc::default(),
        };
        let clock = FakeClock::new(now());
        let outcome = match side {
            Side::OpLock | Side::OpTryClaim => {
                let mut lock =
                    TargetLock::with_clock(Box::new(racing), &reaping_cfg(), clock.clone());
                if matches!(side, Side::OpLock) {
                    lock.lock("").await.map(|()| true)
                } else {
                    lock.try_claim("").await
                }
            }
            Side::PoolLock | Side::PoolTryClaim => {
                let mut p = PoolLock::with_clock(
                    Box::new(racing),
                    &cfg(),
                    "SUSE:Maintenance:1:2",
                    clock.clone(),
                );
                if matches!(side, Side::PoolLock) {
                    p.lock(POOL_COMMENT).await.map(|()| true)
                } else {
                    p.try_claim(POOL_COMMENT).await
                }
            }
        };
        (outcome, clock.monotonic(), handle)
    }

    #[tokio::test]
    async fn a_claimant_whose_line_lands_after_its_create_is_contended_never_invalid() {
        for side in SIDES {
            for age in [Age::Ago(1), Age::Unknown, Age::Future] {
                for reads in [1, 2, 3] {
                    let (outcome, _, handle) =
                        claim_against_empty_file(side, age, Some(reads)).await;
                    let ctx = format!("{side:?} {age:?} reads {reads}");
                    match (side, outcome) {
                        (Side::OpLock | Side::PoolLock, Err(HostError::TargetLocked(msg))) => {
                            assert!(!msg.contains("force"), "{ctx}: {msg}");
                        }
                        (Side::OpTryClaim | Side::PoolTryClaim, Ok(false)) => {}
                        (_, other) => panic!("{ctx}: {other:?}"),
                    }
                    assert_eq!(
                        handle.file_contents(side.path()).unwrap(),
                        side.valid_line()
                    );
                    assert_eq!(removes(&handle, side.path()), 0, "{ctx}");
                }
            }
        }
    }

    #[tokio::test]
    async fn an_old_empty_file_is_invalid_at_once_and_says_how_old_it_is() {
        for side in SIDES {
            let (outcome, elapsed, handle) =
                claim_against_empty_file(side, Age::Ago(600), None).await;
            let Err(err) = outcome else {
                panic!("{side:?}: {outcome:?}");
            };
            assert!(
                matches!(err, HostError::InvalidLockFile { .. }),
                "{side:?}: {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains("last modified 600s ago"), "{side:?}: {msg}");
            assert!(
                msg.contains("if no other mtui is claiming it"),
                "{side:?}: {msg}"
            );
            assert_eq!(elapsed, 0.0, "{side:?} must not wait on an old file");
            assert_eq!(
                creates(&handle, side.path()),
                usize::from(matches!(side, Side::OpLock | Side::PoolLock))
            );
            assert_eq!(handle.file_contents(side.path()).unwrap(), b"");
        }
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_proved_old_is_given_the_settle_window_then_reported_invalid() {
        for side in SIDES {
            for (age, window, said) in [
                (Age::Ago(2), 5.0, "last modified 2s ago"),
                (Age::Unknown, 5.0, "last modification time unknown"),
                (Age::Future, 5.0, "last modification time unknown"),
            ] {
                let (outcome, elapsed, handle) = claim_against_empty_file(side, age, None).await;
                let ctx = format!("{side:?} {age:?}");
                let Err(err) = outcome else {
                    panic!("{ctx}: {outcome:?}");
                };
                assert!(
                    matches!(err, HostError::InvalidLockFile { .. }),
                    "{ctx}: {err:?}"
                );
                assert!(err.to_string().contains(said), "{ctx}: {err}");
                assert_eq!(elapsed, window, "{ctx}");
                assert_eq!(
                    creates(&handle, side.path()),
                    usize::from(matches!(side, Side::OpLock | Side::PoolLock)),
                    "{ctx}"
                );
                assert_eq!(handle.file_contents(side.path()).unwrap(), b"", "{ctx}");
            }
        }
    }

    #[tokio::test]
    async fn a_file_that_vanishes_while_settling_is_claimed_as_free() {
        for side in [Side::OpLock, Side::PoolLock] {
            let path = side.path();
            let inner = MockConnection::new("h1").with_file(path, Vec::new());
            let handle = inner.clone();
            let racing = RacingConnection {
                inner,
                foreign: Arc::default(),
                on_mtime: Arc::default(),
                land_on_open: Arc::new(Mutex::new(Some((1, None)))),
                forwarded_writes: Arc::default(),
                forwarded_removes: Arc::default(),
            };
            let clock = FakeClock::new(now());
            match side {
                Side::PoolLock => {
                    let mut p = PoolLock::with_clock(
                        Box::new(racing),
                        &cfg(),
                        "SUSE:Maintenance:1:2",
                        clock,
                    );
                    p.lock(POOL_COMMENT).await.unwrap();
                }
                _ => {
                    let mut lock = TargetLock::with_clock(Box::new(racing), &reaping_cfg(), clock);
                    lock.lock("").await.unwrap();
                    assert!(lock.holds());
                }
            }
            assert_ne!(handle.file_contents(path).unwrap(), b"", "{side:?}");
        }
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
