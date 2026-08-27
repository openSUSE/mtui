//! Per-client MCP session.
//!
//! [`McpSession`] backs one `mtui-mcp` client: the mutable [`Session`] state a
//! command dispatches against, the [`SharedBuf`] sink capturing its display
//! output, and [`run_command`](McpSession::run_command) — the central dispatch
//! primitive, supplying the [`McpCommandError`] envelope and the
//! `[mcp] max_output_bytes` cap. The non-interactive contract
//! (`interactive = false`, unset prompter) comes from `capture::session` passing
//! `is_repl = false`. stdio has one instance, http one per client, both reached
//! through the [`crate::provider::SessionProvider`] seam so the tool layer stays
//! transport-agnostic.
//!
//! **Per-template lock discipline.** A shared/exclusive registry gate
//! ([`crate::concurrency::RwGate`]) plus a lazily-created per-RRID lock map:
//! `command_lock` takes the gate *shared* + one per-RRID lock for a
//! single-template call, *exclusive* for fan-out and for registry mutators
//! ([`Command::mutates_registry`](mtui_core::Command::mutates_registry)), which
//! must land on the canonical session. The scoped path dispatches a *spawned*
//! [`dispatch_command`] on a [`Session::fork_for_call`] — sharing the reports'
//! per-entry locks, carrying its own display, holding no session-wide mutex — so
//! different-RRID calls get real concurrency and per-call output isolation.
//!
//! **Background jobs.** [`start_jobs`](McpSession::start_jobs) backgrounds a slow
//! `run`/`update`/`downgrade` as one `-T`-scoped job per resolved template,
//! polled and controlled through the `job_*` methods. Each worker goes through
//! [`run_command`](McpSession::run_command), so it takes the same gates and cap
//! as a foreground call. Bounded both ways: a spawn is rejected before allocating
//! a worker once `[mcp] max_active_jobs` are running (a fan-out whole), and
//! terminal records FIFO-evict to `[mcp] max_completed_jobs` (`0` disables
//! either). The capture sink is bounded at *write time* ([`crate::capture`]).
//!
//! **Progress heartbeats.** `run_command_with_progress` races the dispatch
//! against a ticker emitting `notifications/progress` every
//! `DEFAULT_PROGRESS_INTERVAL` to a transport-free [`ProgressSink`], so a client
//! honouring the protocol does not time out. The rmcp-backed sink is built in
//! [`crate::server`], keeping this layer rmcp-free; `None` is zero-overhead.
//!
//! **[`close`](McpSession::close).** Eviction teardown: for **every** loaded
//! template, release pool claims then disconnect the host group — best-effort,
//! idempotent, bounded by [`HOST_CLOSE_TIMEOUT`] so a wedged close cannot block
//! the idle-sweep. Groups keep their now-dead targets, dropped with the report.
//!
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use mtui_config::Config;
use mtui_core::{
    ColorMode, CommandError, CommandPromptDisplay, EngineError, HOST_CLOSE_TIMEOUT, Registry,
    Session, dispatch_argv, dispatch_command, resolve_command_rrids,
};
use mtui_hosts::LockOutcome;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::capture::{self, SharedBuf};
use crate::concurrency::{ExclusiveGuard, RwGate, SharedGuard};
use crate::slim::{cap_output, truncation_notice};

/// Default interval between `notifications/progress` heartbeat frames.
///
/// Not a config key: the tool layer passes it to
/// [`McpSession::run_command_with_progress`], overridable per call so tests can
/// drive a sub-second interval.
pub(crate) const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Cooperative grace [`job_cancel`](McpSession::job_cancel) gives a worker
/// between cancelling its token and force-aborting its task.
///
/// A dispatch parked at a seam checkpoint settles well inside this; a body
/// blocked mid host-op never observes the token and burns the full grace before
/// the hard abort. Kept short so `job_cancel` stays responsive; not a config key.
pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// Wall-clock budget for the whole post-abort operation-lock release
/// ([`McpSession::unlock_after_abort`]), across every template the cancelled
/// job was scoped to.
///
/// A force-aborted dispatch never reached its own `unlock()`, so the cancel
/// releases the operation lock on its behalf — but that is an SSH round-trip per
/// host and may queue behind another template's in-flight dispatch, so it is
/// bounded to keep `job_cancel` responsive. On expiry the remaining locks stay
/// held and the reply says so; the `unlock` command and the fleet's stale-lock
/// reap are the backstop. Not a config key.
pub(crate) const ABORT_UNLOCK_BUDGET: Duration = Duration::from_secs(5);

/// A [`JoinHandle`] wrapper that aborts its task when dropped.
///
/// If the future awaiting a spawned dispatch is itself cancelled (an aborted job
/// worker, a dropped request), this aborts the dispatch too, preserving the
/// inline path's shape: the body is dropped, not detached to run on unobserved.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A transport-free sink for heartbeat progress frames.
///
/// A trait rather than the rmcp `Peer`, so this layer stays transport-free and
/// unit-testable with a recording double; `crate::server::PeerProgressSink` is
/// the rmcp-backed implementation. Implementors **must not** propagate transport
/// failures — a send error is the sink's own concern (log at DEBUG and swallow)
/// so a flaky client can never mask the command's outcome.
///
/// [`report`](ProgressSink::report) returns a boxed future rather than being a
/// native `async fn`, to stay `dyn`-compatible without pulling `async-trait`
/// into this always-compiled library layer.
pub trait ProgressSink: Send + Sync {
    /// Emit one progress frame: `progress` elapsed seconds so far, `message` the
    /// human-readable heartbeat line. `total` is always unknown for a heartbeat.
    fn report<'a>(
        &'a self,
        progress: f64,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Drive `fut` to completion while emitting a heartbeat every `interval`.
///
/// Each tick reports monotonic elapsed seconds and a `"<command> running
/// (<n>s)…"` message; `fut`'s output is returned unchanged and no heartbeat is
/// emitted after completion. The sink swallows its own transport errors (see
/// [`ProgressSink`]), so this loop cannot mask `fut`'s result.
pub(crate) async fn run_with_heartbeat<F>(
    fut: F,
    sink: &dyn ProgressSink,
    command: &str,
    interval: Duration,
) -> F::Output
where
    F: Future,
{
    // `progress` is on the std clock, which tokio's `start_paused` does not
    // advance: a virtual-time test sees 0.0 on every frame, so assertions about
    // *values* must run on the wall clock. Tick *counts* stay exact either way.
    let started = Instant::now();
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Biased so a body finishing exactly on a tick boundary returns
            // instead of emitting a spurious final frame.
            biased;
            output = &mut fut => return output,
            () = tokio::time::sleep(interval) => {
                let elapsed = started.elapsed().as_secs_f64();
                sink.report(elapsed, &format!("{command} running ({elapsed:.0}s)…"))
                    .await;
            }
        }
    }
}

/// A command dispatch that failed under the MCP transport.
///
/// Carries the streams captured during the failed run, with an argparse-style
/// `exit_code`. [`Display`](std::fmt::Display) renders a one-line summary plus
/// the captured stderr, so the default MCP error envelope is human-readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCommandError {
    /// Captured stdout up to the point of failure (already output-capped).
    pub(crate) stdout: String,
    /// Captured stderr (parse/usage text, command-error message).
    pub stderr: String,
    /// Non-zero exit code: `2` for parse/usage errors, `1` otherwise.
    pub exit_code: i32,
}

impl std::fmt::Display for McpCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command failed (exit_code={})", self.exit_code)?;
        let tail = self.stderr.trim();
        if !tail.is_empty() {
            write!(f, ": {tail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for McpCommandError {}

/// The outcome of [`McpSession::run_command_client_cancellable`].
///
/// Separates a dispatch that ran to completion from one the client's cancel
/// forced an abort on, so the server layer can render the forced case's
/// `McpError` with the unlock verdict [`Completed`](Self::Completed) never has.
pub(crate) enum ToolOutcome {
    /// The dispatch returned its own verdict: it finished before the cancel, or
    /// observed it cooperatively and unwound (running its own `unlock()`) inside
    /// the grace period.
    Completed(Result<String, McpCommandError>),
    /// The grace elapsed and the dispatch was force-aborted; carries the
    /// post-abort operation-lock release's outcome.
    Aborted(AbortUnlock),
}

impl From<Result<String, McpCommandError>> for ToolOutcome {
    fn from(result: Result<String, McpCommandError>) -> Self {
        ToolOutcome::Completed(result)
    }
}

/// The lifecycle state of a background job.
///
/// One of `running`/`done`/`failed`/`cancelled`; [`Display`](std::fmt::Display)
/// renders the lowercase token the job tools print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// The worker task is still executing (or queued behind its lock).
    Running,
    /// The command finished successfully; its stdout is in `Job::result`.
    Done,
    /// The command failed; `Job::error`/`Job::exit_code` carry the envelope.
    Failed,
    /// The job was cancelled via [`McpSession::job_cancel`].
    Cancelled,
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

/// One background-job record.
///
/// Shared between the spawned worker (which writes the terminal state/result)
/// and the poll methods, so it lives behind an `Arc<StdMutex<Job>>` in
/// [`McpSession::jobs`]. The `StdMutex` is only ever held for a field
/// read/write, never across an `.await`.
#[derive(Debug)]
struct Job {
    /// Session-unique: `"<command>-<n>"` or `"<command>-<rrid>-<n>"`.
    id: String,
    command: String,
    /// The templates this job's dispatch resolves to, recorded at mint time.
    ///
    /// Scopes the post-abort operation-lock release
    /// ([`McpSession::unlock_after_abort`]) to the templates this job could have
    /// locked hosts on, so cancelling one job never disturbs a concurrent job's
    /// locks on another template. Recorded at mint rather than re-derived at
    /// cancel time: the loaded set may have changed since, and the job id carries
    /// the RRID only as a `:`-mangled string.
    ///
    /// **Empty** means the scope is unknown (the synchronous
    /// [`start_job`](McpSession::start_job) cannot resolve, and an argv resolving
    /// to nothing real has no scope). The cancel path then falls back to every
    /// loaded template — the conservative answer for a dispatch that may have
    /// held the registry gate exclusively and locked hosts across all of them.
    rrids: Vec<String>,
    state: JobState,
    started: Instant,
    /// Freezes `elapsed_s` once terminal.
    finished: Option<Instant>,
    /// Captured stdout, on success or up to the failure.
    result: Option<String>,
    /// The `McpCommandError` stderr when `state == Failed`.
    error: Option<String>,
    exit_code: Option<i32>,
    /// Aborted by [`McpSession::job_cancel`] when the cooperative grace elapses.
    handle: Option<JoinHandle<()>>,
    /// Installed on the session this job's dispatch runs on;
    /// [`McpSession::job_cancel`] fires it *first* so a body observing the seam
    /// can stop cooperatively before the hard abort.
    cancel: CancellationToken,
}

/// A public, poll-facing snapshot of a `Job` (no task handle), which the job
/// tools render into the one-line status text.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    /// The job id.
    pub id: String,
    /// The command name.
    pub command: String,
    /// The lifecycle state.
    pub state: JobState,
    /// Elapsed wall-clock seconds, rounded to 0.1s (frozen once terminal).
    pub(crate) elapsed_s: f64,
}

/// What the post-abort operation-lock release
/// ([`McpSession::unlock_after_abort`]) actually achieved: disjoint buckets
/// rather than a formatted string, so the reply can tell "released" from "left
/// alone" from "failed" from "never got there". A forced cancel must never claim
/// a release it did not perform.
#[derive(Debug, Default)]
pub(crate) struct AbortUnlock {
    /// Hosts whose hold this pass dropped. The fan-out is scoped to locks the
    /// job's own group actually held
    /// ([`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held)), so a
    /// never-locked host is not in the map at all.
    unlocked: Vec<String>,
    /// Hosts whose lock belongs to another owner — left untouched (benign).
    contended: Vec<String>,
    /// Hosts whose release hit a real transport error (with the reason); the
    /// lock is still held there.
    failed: Vec<(String, String)>,
    /// Templates the budget expired on, before or part-way through their host
    /// fan-out. In the second case the dropped fan-out future took its partial
    /// outcome map with it, so the whole template is reported unknown rather
    /// than guessing which half succeeded.
    unknown: Vec<String>,
    /// The budget expired in the preamble, before the scope was even resolved:
    /// the pass never ran and no template can be named.
    stalled: bool,
    /// The budget expired on the **null sentinel's** unlock (hosts attached with
    /// no report loaded). Tracked separately from `unknown` because that bucket
    /// names registry RRIDs and its remedy points at `list_locks -T <rrid>` —
    /// the sentinel has no RRID, so its remedy is a bare `list_locks`/`unlock`.
    null_group_unknown: bool,
}

impl AbortUnlock {
    /// Folds one template's
    /// [`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held) outcome
    /// map into the buckets.
    fn absorb(&mut self, outcomes: BTreeMap<String, LockOutcome>) {
        for (host, outcome) in outcomes {
            match outcome {
                LockOutcome::Released => self.unlocked.push(host),
                LockOutcome::Contended => self.contended.push(host),
                LockOutcome::Failed(reason) => self.failed.push((host, reason)),
                // Unreachable on an unlock fan-out; ignored rather than folded
                // into a bucket (as the `unlock` command's own match does) —
                // inventing a verdict for an impossible outcome is how a reply
                // starts claiming things that did not happen.
                LockOutcome::Acquired => {}
            }
        }
    }

    /// The clause appended to the forced-cancel reply, or `None` when there was
    /// nothing to say — silence is deliberate, so a cancel with no host lock to
    /// act on leaves the reply byte-identical. The remedies are *scoped* for a
    /// related reason: an expired release usually means a **successor** dispatch
    /// already holds the template, and a bare `unlock` there would strip a live
    /// operation's lock.
    pub(crate) fn clause(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.unlocked.is_empty() {
            parts.push(format!("unlocked: {}", self.unlocked.join(", ")));
        }
        if !self.contended.is_empty() {
            parts.push(format!(
                "still locked by another owner: {} (use `unlock --force` if that owner \
                 is a dead mtui)",
                self.contended.join(", ")
            ));
        }
        for (host, reason) in &self.failed {
            parts.push(format!(
                "unlock FAILED on {host} ({reason}); release it with `unlock --force`"
            ));
        }
        if !self.unknown.is_empty() {
            parts.push(format!(
                "lock state unknown on {} (release timed out); check with \
                 `list_locks -T <rrid>` and release with `unlock -T <rrid>` once no \
                 operation is running on that template",
                self.unknown.join(", ")
            ));
        }
        if self.null_group_unknown {
            parts.push(
                "lock state unknown on hosts attached with no report loaded (release \
                 timed out); check with `list_locks` and release with `unlock` while no \
                 template is loaded and no operation is running"
                    .to_owned(),
            );
        }
        if self.stalled {
            parts.push(
                "lock state unknown (the session was busy, so the release never ran); \
                 check with `list_locks` and release with `unlock` once no operation is \
                 running"
                    .to_owned(),
            );
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }
}

/// The shared parenthetical for a forced abort: the grace period, the
/// in-flight-host-operation caveat, and (if any) the unlock verdict.
///
/// Shared between [`McpSession::job_cancel_with_budget`]'s reply and
/// [`crate::server`]'s `cancelled_error`, so both forced-abort surfaces read
/// identically.
pub(crate) fn forced_abort_note(unlocked: &AbortUnlock) -> String {
    let mut note = format!(
        "forced abort after {}s grace; a host operation already in flight may \
         still finish on the host",
        CANCEL_GRACE.as_secs()
    );
    if let Some(clause) = unlocked.clause() {
        note.push_str("; ");
        note.push_str(&clause);
    }
    note
}

/// Pins `argv` to `rrid` by **prepending** `-T <rrid>`.
///
/// Prepended, not appended: a positional `REMAINDER` command like `run` would
/// swallow a trailing `-T <rrid>` into its own value.
///
/// Left untouched when `argv` already carries an explicit scope flag: a second
/// `-T`/`--template` is redundant, and `--all-templates` is declared
/// `conflicts_with("template")`, so adding `-T` would turn the dispatch into a
/// *parse error* instead of scoping it. That job therefore stays unpinned, and
/// its recorded scope can drift if the loaded set changes mid-flight.
fn scope_argv(rrid: &str, argv: &[String]) -> Vec<String> {
    let already_scoped = argv
        .iter()
        .any(|a| a == "-T" || a == "--template" || a == "--all-templates");
    if already_scoped {
        return argv.to_vec();
    }
    let mut scoped = vec!["-T".to_owned(), rrid.to_owned()];
    scoped.extend(argv.iter().cloned());
    scoped
}

/// Process-global monotonic source of [`McpSession::id`] values, so two distinct
/// sessions never share one (freshness independent of heap-address reuse).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// A headless mtui session backing one MCP client.
///
/// The [`Session`] sits behind a [`Mutex`] because dispatch
/// ([`mtui_core::dispatch_argv`]) needs `&mut Session` while the rmcp
/// `ServerHandler` methods take `&self`. The paired [`SharedBuf`] is the sink
/// its display writes to; a tool call [`take`](SharedBuf::take)s it to isolate
/// its own output.
pub struct McpSession {
    /// Asserts session freshness without relying on `Arc` address identity.
    id: u64,
    session: Arc<Mutex<Session>>,
    /// The sink the session's display writes to; drained per tool call.
    output: SharedBuf,
    /// `config.mcp_max_output_bytes`; `0` disables the cap. Copied out of the
    /// config, with the four fields below, because the server holds the session
    /// rather than the [`Config`].
    max_output_bytes: usize,
    /// `config.mcp_max_input_bytes`; `0` disables the cap. Bounds how much of an
    /// on-disk checkout file `testreport_read` reads before stopping.
    max_input_bytes: usize,
    /// `config.mcp_profile`, consumed by
    /// [`McpServer::new`](crate::server::McpServer::new) to narrow the tools.
    profile: String,
    /// `config.mcp_tools_allow`: extra tools kept on top of the profile.
    tools_allow: Vec<String>,
    /// `config.mcp_tools_deny`: tools removed regardless of profile/allow.
    tools_deny: Vec<String>,
    /// The registry shared/exclusive gate: *shared* for a single-template
    /// command (so it cannot overlap a registry mutation), *exclusive* for
    /// registry mutators and unscoped fan-out, draining in-flight per-RRID work.
    /// See [`command_lock`](Self::command_lock).
    gate: RwGate,
    /// Lazily-created per-RRID locks: same-RRID calls share one and serialise,
    /// different-RRID calls take different ones. The outer [`StdMutex`] guards
    /// the lazy get-or-insert only, never held across an await.
    rrid_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The background-job table, keyed by job id.
    ///
    /// Each worker records its outcome on its own `Arc<StdMutex<Job>>`, so the
    /// poll methods read it without locking the session. The outer [`StdMutex`]
    /// guards insert/lookup/eviction only, never held across an await.
    jobs: StdMutex<HashMap<String, Arc<StdMutex<Job>>>>,
    /// Pre-incremented per minted job, so ids are session-unique.
    job_counter: AtomicU64,
    /// `config.mcp_max_active_jobs`: a spawn exceeding it is rejected before the
    /// worker is allocated. `0` disables the cap.
    max_active_jobs: usize,
    /// `config.mcp_max_completed_jobs`: terminal records beyond it are evicted
    /// oldest-finished-first. `0` disables the cap.
    max_completed_jobs: usize,
}

/// An acquired hold on the concurrency gate for one command/tool invocation.
///
/// Returned by `McpSession::command_lock` / [`McpSession::scoped_lock`] and kept
/// alive for the critical section; dropping it releases the gate (and any
/// per-RRID lock) in the right order. The fields exist only to own the guards,
/// hence the leading underscores.
#[must_use = "dropping the CommandLock immediately releases the gate"]
pub enum CommandLock {
    /// A single-template hold: the registry gate shared **plus** one per-RRID
    /// lock. Declaration order drops `_rrid` first, then `_shared`, the reverse
    /// of the acquire order.
    Scoped {
        /// The per-RRID lock (dropped first).
        _rrid: OwnedMutexGuard<()>,
        /// The registry gate held in shared mode (dropped second).
        _shared: SharedGuard,
    },
    /// A registry-wide exclusive hold (mutators / unscoped fan-out).
    Exclusive(#[allow(dead_code)] ExclusiveGuard),
}

impl McpSession {
    /// Builds a headless session from `config`, wiring its display to a fresh
    /// capture sink. Non-interactive with color disabled — see
    /// `capture::session`.
    #[must_use]
    pub fn new(config: Config) -> Arc<Self> {
        let max_output_bytes = config.mcp_max_output_bytes;
        let max_input_bytes = config.mcp_max_input_bytes;
        let profile = config.mcp_profile.clone();
        let tools_allow = config.mcp_tools_allow.clone();
        let tools_deny = config.mcp_tools_deny.clone();
        let max_active_jobs = config.mcp_max_active_jobs;
        let max_completed_jobs = config.mcp_max_completed_jobs;
        let (session, output) = capture::session(config);
        Arc::new(Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            session: Arc::new(Mutex::new(session)),
            output,
            max_output_bytes,
            max_input_bytes,
            profile,
            tools_allow,
            tools_deny,
            gate: RwGate::new(),
            rrid_locks: StdMutex::new(HashMap::new()),
            jobs: StdMutex::new(HashMap::new()),
            job_counter: AtomicU64::new(0),
            max_active_jobs,
            max_completed_jobs,
        })
    }

    /// The process-unique, monotonic id assigned at construction — a valid
    /// freshness signal where `Arc` address identity is not, since a freed
    /// address can be reused by the allocator.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The guarded session, for dispatch under the session lock.
    #[must_use]
    pub fn session(&self) -> &Arc<Mutex<Session>> {
        &self.session
    }

    /// The capture sink, drained per tool call to isolate that call's output.
    #[must_use]
    pub fn output(&self) -> &SharedBuf {
        &self.output
    }

    /// The per-result output-size budget in bytes (`0` disables the cap).
    ///
    /// Exposed for [`crate::testreport_tools`], which cap their file-content
    /// payloads with the same [`cap_output`] budget.
    #[must_use]
    pub(crate) fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// The configured source read-size budget (bytes); `0` disables it.
    ///
    /// Exposed for [`testreport_read`](crate::testreport_tools), which stops at
    /// this many bytes (appending a truncation notice) so a huge or slow checkout
    /// file cannot exhaust memory.
    #[must_use]
    pub(crate) fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    /// The configured tool-surface profile (`full` / `core`), consumed by
    /// [`McpServer::new`](crate::server::McpServer::new).
    #[must_use]
    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    /// Extra tool names to keep on top of the profile.
    #[must_use]
    pub(crate) fn tools_allow(&self) -> &[String] {
        &self.tools_allow
    }

    /// Tool names to remove regardless of profile/allow.
    #[must_use]
    pub(crate) fn tools_deny(&self) -> &[String] {
        &self.tools_deny
    }

    /// Returns (creating on first use) the per-template lock for `rrid`,
    /// populated under the map's guard so two tasks racing to lock the same fresh
    /// RRID share one lock object.
    fn lock_for(&self, rrid: &str) -> Arc<Mutex<()>> {
        let mut map = self.rrid_locks.lock().expect("rrid lock map poisoned");
        Arc::clone(
            map.entry(rrid.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Acquires the right lock(s) for a `name`/`argv` invocation, resolving
    /// exactly as the foreground dispatch does (via [`resolve_command_rrids`]):
    ///
    /// * **exactly one** loaded template → the gate *shared* **plus** that
    ///   template's per-RRID lock, so different-RRID commands run concurrently
    ///   while same-RRID ones serialise and none overlaps a registry mutation;
    /// * fan-out / unscoped-multi, registry mutators, or anything resolving to no
    ///   real template → the gate *exclusive*, draining in-flight per-RRID
    ///   commands and blocking new ones for the duration.
    ///
    /// A single call never holds two per-RRID locks and the exclusive path holds
    /// only the gate, so the lock order (gate-shared → one rrid lock) is total and
    /// cannot deadlock. Resolution briefly locks the session, released before the
    /// guard is handed back so the caller may re-lock it for dispatch.
    async fn command_lock(&self, registry: &Registry, name: &str, argv: &[String]) -> CommandLock {
        let rrids = match registry.get(name) {
            // Exclusive even when it resolves to a single template: the
            // concurrent path dispatches on a per-call fork whose registry
            // snapshot is discarded, so a structural mutation would be lost
            // unless it runs against the canonical session.
            Some(command) if command.mutates_registry() => None,
            Some(command) => {
                let session = self.session.lock().await;
                resolve_command_rrids(command.as_ref(), &session, argv)
            }
            // Unknown command: no meaningful scope, so serialise conservatively.
            None => None,
        };

        match rrids {
            Some(rrids) if rrids.len() == 1 => {
                let shared = self.gate.shared().await;
                let lock = self.lock_for(&rrids[0]);
                let rrid = lock.lock_owned().await;
                CommandLock::Scoped {
                    _shared: shared,
                    _rrid: rrid,
                }
            }
            _ => CommandLock::Exclusive(self.gate.exclusive().await),
        }
    }

    /// The registry gate in exclusive mode: the hold the hand-written transfer
    /// tools (`get`/`put`, #434) take around their host fan-outs, matching
    /// [`command_lock`](Self::command_lock)'s `_ =>` arm. `Session::activate` may
    /// only be flipped under the exclusive gate.
    pub(crate) async fn exclusive_lock(&self) -> CommandLock {
        CommandLock::Exclusive(self.gate.exclusive().await)
    }

    /// Holds the registry-shared gate plus one template's per-RRID lock.
    ///
    /// For the hand-written testreport tools, which act on one template's files:
    /// the shared gate keeps the loaded set stable for the body while still
    /// letting tools on *other* templates run in parallel, and the per-RRID lock
    /// serialises against foreground dispatch for the *same* template.
    ///
    /// `rrid` is the resolved target template id, or `None` to fall back to the
    /// active one. Callers should resolve and validate the target report *inside*
    /// the body, where the shared gate guarantees the registry cannot change
    /// underfoot.
    pub async fn scoped_lock(&self, rrid: Option<&str>) -> CommandLock {
        let shared = self.gate.shared().await;
        let key = match rrid {
            Some(r) => r.to_owned(),
            None => self
                .session
                .lock()
                .await
                .templates
                .active_rrid()
                .unwrap_or("")
                .to_owned(),
        };
        let lock = self.lock_for(&key);
        let rrid = lock.lock_owned().await;
        CommandLock::Scoped {
            _shared: shared,
            _rrid: rrid,
        }
    }

    /// Releases pool claims and disconnects every loaded template's hosts.
    ///
    /// Mirrors the REPL `quit` disconnect path but **without** its exit-flag /
    /// history-flush tail, since the process keeps serving other clients.
    ///
    /// **Every** connected host goes, not just the active template's: a session
    /// may hold several templates, and hosts attached while nothing was loaded
    /// live in the null-report group. See
    /// [`Session::take_teardown_units`](mtui_core::Session::take_teardown_units).
    /// The timeout covers the mutex-taking preamble too, so a busy session is
    /// abandoned rather than hanging the idle-sweep awaiting this.
    pub async fn close(&self) {
        self.close_with_timeout(HOST_CLOSE_TIMEOUT).await;
    }

    /// [`close`](Self::close) with an explicit fan-out budget.
    ///
    /// The timeout seam exists so the colocated wedged-close unit test can bound
    /// the wait to a fraction of a second instead of [`HOST_CLOSE_TIMEOUT`].
    async fn close_with_timeout(&self, timeout: Duration) {
        // Armed before the preamble, as in `unlock_after_abort`: the
        // mutex-taking preamble must be inside the budget too.
        let deadline = Instant::now() + timeout;

        // The session guard is dropped *before* the teardown awaits: holding a
        // `MutexGuard<Session>` across the per-entry `.await` would force the
        // close future to require `Session: Sync`, which it is not (the display
        // sink is `Send`-only). The handles keep each report alive on their own.
        let preamble = async { self.session.lock().await.take_teardown_units() };
        let Ok(handles) = tokio::time::timeout(timeout, preamble).await else {
            // Unlike `unlock_after_abort`'s give-up, this one strands real work:
            // every host stays connected with its remote `/var/lock/mtui.lock`
            // held until some later call reaches the same session and retries.
            tracing::warn!(
                ?timeout,
                "close: session busy past the budget; host teardown abandoned entirely"
            );
            return;
        };

        for entry in handles {
            let left = deadline.saturating_duration_since(Instant::now());
            let unit = async {
                let mut report = entry.lock().await;
                report.release_pool_claims().await;
                // Plain disconnect: no reboot/poweroff on an eviction, unlike
                // the REPL `quit` bootarg, and per-host outcomes are irrelevant.
                let _ = report.base_mut().targets.close(None).await;
            };
            // Each unit only gets what remains of the shared deadline, so the
            // fan-out stays bounded by `timeout` regardless of unit count.
            if tokio::time::timeout(left, unit).await.is_err() {
                tracing::warn!(
                    ?timeout,
                    "host disconnect timed out; abandoning this report's teardown"
                );
            }
        }
    }

    /// Runs a registered command and returns its captured, output-capped stdout.
    ///
    /// The central MCP dispatch primitive: `name`/`argv` go through the **same**
    /// engine the REPL uses, under this call's `command_lock`, on the scoped or
    /// the exclusive path (see the body). A `--help`/`--version` request is a
    /// *success*, matching argparse's exit-0 semantics.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] on a parse failure (`exit_code == 2`), an unknown
    /// command or a failing body (`1`), carrying the capped stdout produced
    /// before the failure plus the failure text as stderr.
    pub async fn run_command(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
    ) -> Result<String, McpCommandError> {
        self.run_command_cancellable(registry, name, argv, None)
            .await
    }

    /// [`run_command`](Self::run_command) with an optional per-job cancellation
    /// token installed on the session the dispatch runs on.
    ///
    /// The background-job worker passes its job's token so
    /// [`job_cancel`](Self::job_cancel) can cancel exactly that dispatch;
    /// foreground tool calls pass `None` and get a fresh, never-cancelled one.
    /// The token is set on the per-call fork on the scoped path, and on the
    /// canonical session (restored after) on the exclusive one.
    async fn run_command_cancellable(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        cancel: Option<CancellationToken>,
    ) -> Result<String, McpCommandError> {
        // Taken *before* touching the session, so same-RRID and unscoped calls
        // serialise and mutators drain in-flight per-RRID work.
        let lock = self.command_lock(registry, name, argv).await;

        // Per-call output isolation: its own capture buffer + display, so two
        // overlapping calls never clobber each other's stdout. Bounded to the
        // same budget as the session-wide sink.
        let call_buf = SharedBuf::with_limit(self.max_output_bytes);
        let call_display =
            CommandPromptDisplay::with_sink(Box::new(call_buf.clone()), ColorMode::Never);

        let result = match &lock {
            // Concurrent path. The fork *shares* the reports' per-entry locks, so
            // this call locks only its own template's entry, and the canonical
            // mutex is not held across the dispatch — that is what lets a
            // different-RRID call run in parallel. Content mutations stay visible
            // to the canonical session (same `Arc<Mutex<..>>`); the fork's own
            // config/registry structure is discarded, sound because a per-RRID
            // command never mutates them (that is the exclusive path below).
            CommandLock::Scoped { .. } => {
                // *Spawned*, because the caller drives us via `join!` on one
                // task and only a separate task yields real parallelism — hence
                // the owned `Arc<dyn Command>` and cloned argv, making the future
                // `Send + 'static`. The scoped lock already proved the command
                // resolves to exactly one loaded template.
                let command = registry
                    .get(name)
                    .expect("scoped lock implies a resolvable command")
                    .clone();
                let mut call_session = {
                    let session = self.session.lock().await;
                    session.fork_for_call(call_display)
                };
                // Installed **unconditionally**, never inherited: a hard-aborted
                // exclusive dispatch can leave a cancelled token on the canonical
                // session (its restore is skipped when the worker future is
                // dropped), and a fork inheriting that would die at its own
                // pre-flight check.
                call_session.set_cancel_token(cancel.clone().unwrap_or_default());
                let argv_owned = argv.to_vec();
                let mut handle = AbortOnDrop(tokio::spawn(async move {
                    dispatch_command(command.as_ref(), &mut call_session, &argv_owned).await
                }));
                match (&mut handle.0).await {
                    Ok(result) => result,
                    // A panic in the spawned dispatch surfaces as an engine
                    // command error rather than tearing the session down.
                    Err(join_err) => Err(EngineError::Command(CommandError::Other(format!(
                        "dispatch task failed: {join_err}"
                    )))),
                }
            }
            // Exclusive path: no concurrent readers, so dispatch against the
            // canonical session, whose config/registry-structure mutations must
            // persist. The active guard is released afterwards because
            // `Command::run` re-installs one on the active entry as it returns,
            // and a lingering guard would fail a later concurrent forked call's
            // `try_lock_owned` in `activate`. Each call re-establishes its own.
            CommandLock::Exclusive(_) => {
                let mut session = self.session.lock().await;
                let prev_display = std::mem::replace(&mut session.display, call_display);
                // Installed unconditionally rather than swapped-and-restored, for
                // the same self-healing reason as the scoped path above.
                session.set_cancel_token(cancel.clone().unwrap_or_default());
                let result = dispatch_argv(registry, &mut session, name, argv).await;
                // Best-effort; skipped when the worker future is dropped
                // mid-dispatch, which is what the install above heals.
                session.set_cancel_token(CancellationToken::new());
                session.display = prev_display;
                session.release_active_guard();
                result
            }
        };

        // The sink already bounded the output at write time, discarding overflow
        // before it was ever buffered; if it dropped anything, append the notice
        // `cap_output` would have, once, with the write-time overrun count.
        // Otherwise the text is within budget and `cap_output` is a no-op.
        let (captured, dropped) = call_buf.take_with_dropped();
        let text = if dropped > 0 {
            let mut t = captured;
            t.push_str(&truncation_notice(dropped, self.max_output_bytes));
            t
        } else {
            cap_output(captured, self.max_output_bytes)
        };

        match result {
            Ok(()) => Ok(text),
            // argparse-exit-0. clap renders help into the `Parse` message rather
            // than the display sink, so surface that; a genuine usage error is
            // exit 2 below.
            Err(EngineError::Parse {
                help_or_version: true,
                message,
            }) => Ok(cap_output(message, self.max_output_bytes)),
            Err(err) => {
                let (stderr, exit_code) = match &err {
                    EngineError::Parse { message, .. } => (message.clone(), 2),
                    other => (other.to_string(), 1),
                };
                Err(McpCommandError {
                    stdout: text,
                    stderr,
                    exit_code,
                })
            }
        }
    }

    /// [`run_command`](Self::run_command) with optional progress heartbeats.
    ///
    /// With `Some` sink the whole dispatch, lock wait included, is raced against
    /// a heartbeat firing every `interval` so a slow call does not time the
    /// client out; `None` is [`run_command`](Self::run_command) verbatim.
    ///
    /// # Errors
    ///
    /// Propagates [`McpCommandError`] unchanged; the heartbeat path never alters
    /// the command's result.
    pub(crate) async fn run_command_with_progress(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        sink: Option<&dyn ProgressSink>,
        interval: Duration,
    ) -> Result<String, McpCommandError> {
        self.run_command_cancellable_with_progress(registry, name, argv, sink, interval, None)
            .await
    }

    /// [`run_command_with_progress`](Self::run_command_with_progress) with an
    /// optional per-call cancellation token installed on the session the
    /// dispatch runs on (see [`run_command_cancellable`](Self::run_command_cancellable)).
    ///
    /// [`run_command_client_cancellable`](Self::run_command_client_cancellable)
    /// is the only caller that passes `Some`.
    async fn run_command_cancellable_with_progress(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        sink: Option<&dyn ProgressSink>,
        interval: Duration,
        cancel: Option<CancellationToken>,
    ) -> Result<String, McpCommandError> {
        match sink {
            None => {
                self.run_command_cancellable(registry, name, argv, cancel)
                    .await
            }
            Some(sink) => {
                run_with_heartbeat(
                    self.run_command_cancellable(registry, name, argv, cancel),
                    sink,
                    name,
                    interval,
                )
                .await
            }
        }
    }

    /// [`run_command_with_progress`](Self::run_command_with_progress) driven
    /// against the MCP client's own `notifications/cancelled`, with the same
    /// two-stage contract as [`job_cancel`](Self::job_cancel): cooperative signal
    /// → [`CANCEL_GRACE`] → forced abort → best-effort operation-lock release.
    ///
    /// Only a synthesised **command** tool can hold `/var/lock/mtui.lock`
    /// (testreport/transfer tools do not dispatch through the engine at all), so
    /// this is the one call site the server layer routes here rather than through
    /// the bare `cancellable`.
    ///
    /// A cooperative stop inside the grace unwinds the dispatch's own flow, which
    /// runs its own `unlock()`, so [`ToolOutcome::Completed`] carries that flow's
    /// verdict exactly as an uncancelled call would; only a forced abort yields
    /// [`ToolOutcome::Aborted`].
    ///
    /// The post-abort scope resolution gets only a quarter of
    /// [`ABORT_UNLOCK_BUDGET`]: it locks the session, which a concurrent
    /// *exclusive* dispatch can hold for minutes, so a slow resolve falls back to
    /// the empty scope (every loaded template) rather than eating the budget this
    /// method's `job_cancel`-grade responsiveness depends on.
    pub(crate) async fn run_command_client_cancellable(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        sink: Option<&dyn ProgressSink>,
        interval: Duration,
        client_ct: &CancellationToken,
    ) -> ToolOutcome {
        let token = CancellationToken::new();
        let mut fut = Box::pin(self.run_command_cancellable_with_progress(
            registry,
            name,
            argv,
            sink,
            interval,
            Some(token.clone()),
        ));
        tokio::select! {
            biased;
            r = &mut fut => return ToolOutcome::Completed(r),
            () = client_ct.cancelled() => {}
        }
        // Cooperative stage: signal the seam before touching the future.
        token.cancel();
        if let Ok(r) = tokio::time::timeout(CANCEL_GRACE, &mut fut).await {
            return ToolOutcome::Completed(r);
        }
        // Forced stage. The future is dropped *before* the unlock pass because on
        // the exclusive path it holds the canonical session mutex for its whole
        // life, so the pass's preamble would otherwise time out as `stalled`.
        drop(fut);
        let rrids = tokio::time::timeout(
            ABORT_UNLOCK_BUDGET / 4,
            self.resolve_job_rrids(registry, name, argv),
        )
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        ToolOutcome::Aborted(self.unlock_after_abort(&rrids, ABORT_UNLOCK_BUDGET).await)
    }

    /// Resolve the target RRIDs for a backgrounded fan-out, exactly as the
    /// foreground dispatch does (via [`resolve_command_rrids`], applying the
    /// command's own [`Scope`](mtui_core::Scope) against the loaded set), so the
    /// two match. `None` means resolution is not meaningful (unparseable argv, or
    /// only the Null report resolves) and the caller keeps the single-job path.
    async fn resolve_job_rrids(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
    ) -> Option<Vec<String>> {
        let command = registry.get(name)?;
        let session = self.session.lock().await;
        resolve_command_rrids(command.as_ref(), &session, argv)
    }

    /// Reject a spawn of `n` new jobs when it would breach the active cap.
    ///
    /// Enforced against the *projected* running count so a fan-out is admitted or
    /// rejected whole. Must be called holding `jobs_guard`, so the count and the
    /// subsequent inserts are atomic against a concurrent (http) spawn.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) naming the active/max counts.
    fn admit(
        &self,
        jobs_guard: &HashMap<String, Arc<StdMutex<Job>>>,
        n: usize,
    ) -> Result<(), McpCommandError> {
        if self.max_active_jobs == 0 {
            return Ok(());
        }
        let active = jobs_guard
            .values()
            .filter(|j| j.lock().expect("job record poisoned").state == JobState::Running)
            .count();
        if active + n > self.max_active_jobs {
            return Err(McpCommandError {
                stdout: String::new(),
                stderr: format!(
                    "too many active jobs ({active}/{max}); wait for one to finish \
                     or cancel one before starting {n} more",
                    max = self.max_active_jobs,
                ),
                exit_code: 1,
            });
        }
        Ok(())
    }

    /// Create, register and start one worker for `argv`, inserting it into the
    /// already-locked `jobs_guard` and returning its id.
    ///
    /// The worker runs through [`run_command`](Self::run_command), records the
    /// terminal state/result on the job's `Arc<StdMutex<Job>>`, and FIFO-evicts
    /// terminal records past the completed cap on settling. `self` is an `Arc`
    /// because the spawned task must own the session for its `'static` lifetime.
    ///
    /// `rrids` is the caller's already-computed template scope for `argv` (see
    /// [`Job::rrids`]), empty when it could not resolve one.
    fn mint_job(
        self: &Arc<Self>,
        jobs_guard: &mut HashMap<String, Arc<StdMutex<Job>>>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
        job_id: String,
        rrids: Vec<String>,
    ) -> String {
        let cancel = CancellationToken::new();
        let job = Arc::new(StdMutex::new(Job {
            id: job_id.clone(),
            command: name.to_owned(),
            rrids,
            state: JobState::Running,
            started: Instant::now(),
            finished: None,
            result: None,
            error: None,
            exit_code: None,
            handle: None,
            cancel: cancel.clone(),
        }));
        jobs_guard.insert(job_id.clone(), Arc::clone(&job));

        let session = Arc::clone(self);
        let name = name.to_owned();
        let worker_job = Arc::clone(&job);
        let handle = tokio::spawn(async move {
            let outcome = session
                .run_command_cancellable(&registry, &name, &argv, Some(cancel))
                .await;
            {
                let mut j = worker_job.lock().expect("job record poisoned");
                // A cancel may have already marked the record terminal; if so, do
                // not overwrite it with the (aborted) worker's outcome.
                if j.state == JobState::Running {
                    match outcome {
                        Ok(out) => {
                            j.result = Some(out);
                            j.state = JobState::Done;
                        }
                        Err(err) => {
                            j.state = JobState::Failed;
                            j.result = Some(err.stdout);
                            j.error = Some(err.stderr);
                            j.exit_code = Some(err.exit_code);
                        }
                    }
                    j.finished = Some(Instant::now());
                } else if j.state == JobState::Cancelled && j.error.is_none() {
                    // The cancel claimed the record, but a cooperative stop still
                    // produced a verdict naming what the flow managed to do.
                    // Record it (without rewriting the settled state) so
                    // `job_result` can hand back more than "was cancelled".
                    if let Err(err) = outcome {
                        j.error = Some(err.stderr);
                        if !err.stdout.is_empty() {
                            j.result = Some(err.stdout);
                        }
                    }
                }
            }
            session.evict_completed();
        });
        job.lock().expect("job record poisoned").handle = Some(handle);
        job_id
    }

    /// FIFO-evict terminal job records beyond
    /// [`max_completed_jobs`](Self::max_completed_jobs), keeping the
    /// newest-`finished` ones. Running jobs are never evicted. Runs under the
    /// jobs lock, never across an await.
    fn evict_completed(&self) {
        if self.max_completed_jobs == 0 {
            return;
        }
        let mut jobs = self.jobs.lock().expect("jobs table poisoned");
        let mut terminal: Vec<(Instant, String)> = jobs
            .values()
            .filter_map(|j| {
                let j = j.lock().expect("job record poisoned");
                j.finished
                    .filter(|_| j.state != JobState::Running)
                    .map(|f| (f, j.id.clone()))
            })
            .collect();
        if terminal.len() <= self.max_completed_jobs {
            return;
        }
        terminal.sort_by_key(|(finished, _)| *finished);
        let evict = terminal.len() - self.max_completed_jobs;
        for (_, id) in terminal.into_iter().take(evict) {
            jobs.remove(&id);
        }
    }

    /// Start `name`/`argv` in the background and return its job id.
    ///
    /// Mints exactly **one** job (id `"<command>-<n>"`) and returns immediately.
    /// The tool layer calls [`start_jobs`](Self::start_jobs) instead, for one job
    /// per template; this is the primitive for tests and non-fan-out callers.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1), spawning no worker, when the session is
    /// already at `max_active_jobs` running jobs.
    pub fn start_job(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
    ) -> Result<String, McpCommandError> {
        // Synchronous, so it cannot resolve the template scope (that needs the
        // session lock): the job records an empty scope and a forced cancel falls
        // back to every loaded template. See `Job::rrids`.
        self.start_job_scoped(registry, name, argv, Vec::new())
    }

    /// [`start_job`](Self::start_job) with the caller's already-resolved
    /// template scope recorded on the job (see [`Job::rrids`]).
    ///
    /// # Errors
    ///
    /// As [`start_job`](Self::start_job).
    fn start_job_scoped(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
        rrids: Vec<String>,
    ) -> Result<String, McpCommandError> {
        let mut jobs = self.jobs.lock().expect("jobs table poisoned");
        self.admit(&jobs, 1)?;
        let n = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let job_id = format!("{name}-{n}");
        Ok(self.mint_job(&mut jobs, registry, name, argv, job_id, rrids))
    }

    /// Start `name`/`argv` in the background, fanning out one job per template.
    ///
    /// Resolves the target templates as the foreground path does and mints one
    /// `-T`-scoped job each, so a backgrounded fan-out is independently
    /// observable and cancellable per template. A single template (or none)
    /// yields one job with the unchanged `<command>-<n>` id.
    ///
    /// The single-template job is `-T`-scoped too, because resolution happens at
    /// mint and dispatch later: a `load_template` in between would otherwise
    /// widen an unscoped dispatch beyond what the job record names, and a cancel
    /// would strand the unrecorded template's locks behind a success-shaped
    /// reply.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) when spawning the resolved jobs would breach
    /// `max_active_jobs`; the whole fan-out is rejected atomically.
    pub async fn start_jobs(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
    ) -> Result<Vec<String>, McpCommandError> {
        let rrids = self.resolve_job_rrids(&registry, name, &argv).await;
        match rrids {
            Some(rrids) if rrids.len() > 1 => {
                let mut jobs = self.jobs.lock().expect("jobs table poisoned");
                self.admit(&jobs, rrids.len())?;
                Ok(rrids
                    .into_iter()
                    .map(|rrid| {
                        let n = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
                        let token = rrid.replace(':', "_");
                        let job_id = format!("{name}-{token}-{n}");
                        let scoped_argv = scope_argv(&rrid, &argv);
                        self.mint_job(
                            &mut jobs,
                            Arc::clone(&registry),
                            name,
                            scoped_argv,
                            job_id,
                            vec![rrid],
                        )
                    })
                    .collect())
            }
            // One real template: keep the single-job path and its stable id
            // shape, but scope argv and record the RRID so the dispatch cannot
            // drift from what the job says it targets.
            Some(rrids) => {
                let rrid = rrids.into_iter().next().expect("len == 1");
                let scoped_argv = scope_argv(&rrid, &argv);
                Ok(vec![self.start_job_scoped(
                    registry,
                    name,
                    scoped_argv,
                    vec![rrid],
                )?])
            }
            // Nothing real resolves: no template to pin to or record.
            None => Ok(vec![self.start_job_scoped(
                registry,
                name,
                argv,
                Vec::new(),
            )?]),
        }
    }

    /// A poll-facing snapshot: `elapsed_s` frozen at `finished` once terminal,
    /// else measured to now, rounded to 0.1s.
    fn view(job: &Job) -> JobView {
        let end = job.finished.unwrap_or_else(Instant::now);
        let elapsed = (end.duration_since(job.started).as_secs_f64() * 10.0).round() / 10.0;
        JobView {
            id: job.id.clone(),
            command: job.command.clone(),
            state: job.state,
            elapsed_s: elapsed,
        }
    }

    /// Return a view of every job started in this session.
    #[must_use]
    pub fn job_list(&self) -> Vec<JobView> {
        self.jobs
            .lock()
            .expect("jobs table poisoned")
            .values()
            .map(|j| Self::view(&j.lock().expect("job record poisoned")))
            .collect()
    }

    /// Return `job_id`'s state view, or an error if unknown.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) with `"no such job: <id>"` when `job_id` is
    /// not in the table.
    pub fn job_status(&self, job_id: &str) -> Result<JobView, McpCommandError> {
        let job = self.job(job_id)?;
        Ok(Self::view(&job.lock().expect("job record poisoned")))
    }

    /// Return a finished job's stdout, or the right failure envelope.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] when the id is unknown, the job is still running
    /// (pointing the caller at `job_status`), it failed (carrying its captured
    /// stdout / error / exit code), or it was cancelled.
    pub fn job_result(&self, job_id: &str) -> Result<String, McpCommandError> {
        let job = self.job(job_id)?;
        let job = job.lock().expect("job record poisoned");
        match job.state {
            JobState::Running => {
                let elapsed = (Instant::now().duration_since(job.started).as_secs_f64() * 10.0)
                    .round()
                    / 10.0;
                Err(McpCommandError {
                    stdout: String::new(),
                    stderr: format!("job {job_id} still running ({elapsed}s); poll job_status"),
                    exit_code: 1,
                })
            }
            JobState::Failed => Err(McpCommandError {
                stdout: job.result.clone().unwrap_or_default(),
                stderr: job.error.clone().unwrap_or_else(|| "job failed".to_owned()),
                exit_code: job.exit_code.unwrap_or(1),
            }),
            // Surface the cooperative stop's own verdict where the flow produced
            // one; a forced abort has none and keeps the bare form.
            JobState::Cancelled => Err(McpCommandError {
                stdout: job.result.clone().unwrap_or_default(),
                stderr: job.error.as_ref().map_or_else(
                    || format!("job {job_id} was cancelled"),
                    |detail| format!("job {job_id} was cancelled: {detail}"),
                ),
                exit_code: 1,
            }),
            JobState::Done => Ok(job.result.clone().unwrap_or_default()),
        }
    }

    /// Cancel a running job; error if the id is unknown.
    ///
    /// Truthful and two-stage: the job's [`CancellationToken`] is cancelled
    /// first, then the worker task is aborted if the cooperative grace elapses.
    /// The underlying SSH/subprocess operation may still run to completion on the
    /// host — the same caveat as interrupting a foreground `run` — so the reply
    /// says the abort was forced.
    ///
    /// A forced abort drops the dispatch's future mid-`await`, so the operation's
    /// own `unlock()` never runs and `/var/lock/mtui.lock` would be stranded on
    /// every host of every template the job was scoped to. The forced arm
    /// therefore releases it on the job's behalf and reports the per-host outcome;
    /// the cooperative arm does **not**, since a body that unwound through its own
    /// flow ran its own unlock discipline. Only locks the job's **own** host group
    /// took are released, never a comment-marked exclusive reservation — see
    /// [`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held).
    ///
    /// Releasing under this uncertainty matches the command-timeout path, which
    /// also unlocks unconditionally. A package transaction stays serialised by
    /// the package manager's own system-wide lock, and `/var/lock/mtui.lock` is
    /// mtui's coordination layer on top, so leaving it behind blocks every other
    /// tester on those hosts. A `run` or `reboot` has no such second layer, so
    /// there the release is purely mtui-side bookkeeping and the remote command
    /// may still be executing. Nothing else is done at the hosts — no reboot, no
    /// downgrade, no disconnect.
    ///
    /// A job already terminal is **not** re-cancelled: the reply names its actual
    /// state instead of claiming a cancellation that never happened.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) with `"no such job: <id>"` when unknown.
    pub async fn job_cancel(&self, job_id: &str) -> Result<String, McpCommandError> {
        self.job_cancel_with_budget(job_id, ABORT_UNLOCK_BUDGET)
            .await
    }

    /// [`job_cancel`](Self::job_cancel) with an explicit post-abort unlock
    /// budget.
    ///
    /// The timeout seam exists for the same reason
    /// [`close_with_timeout`](Self::close_with_timeout)'s does: the colocated
    /// budget-expiry test bounds the wait to a fraction of a second.
    ///
    /// # Errors
    ///
    /// As [`job_cancel`](Self::job_cancel).
    async fn job_cancel_with_budget(
        &self,
        job_id: &str,
        unlock_budget: Duration,
    ) -> Result<String, McpCommandError> {
        let job = self.job(job_id)?;
        let (handle, token, rrids) = {
            let mut j = job.lock().expect("job record poisoned");
            match j.state {
                JobState::Running => {
                    // Claim the cancel atomically: marking the record terminal
                    // here stops the worker's terminal-write branch (which checks
                    // `Running`) from overwriting it.
                    j.state = JobState::Cancelled;
                    j.finished = Some(Instant::now());
                    (j.handle.take(), j.cancel.clone(), j.rrids.clone())
                }
                state => {
                    // Truthful no-op: nothing was cancelled.
                    return Ok(format!("job {job_id} already {state}; nothing to cancel"));
                }
            }
        };
        // Cooperative stage: signal the seam before touching the task.
        token.cancel();
        let mut forced = false;
        let mut unlocked = AbortUnlock::default();
        if let Some(mut handle) = handle {
            if tokio::time::timeout(CANCEL_GRACE, &mut handle)
                .await
                .is_err()
            {
                // Forced stage: the body never reached a checkpoint.
                forced = true;
                handle.abort();
                // Awaited so cancellation has fully unwound before returning (a
                // `JoinError::Cancelled` is expected) — and so the worker's
                // `CommandLock` is released before the fan-out takes those holds.
                let _ = handle.await;
                unlocked = self.unlock_after_abort(&rrids, unlock_budget).await;
            }
            // The worker's terminal-write branch skipped its eviction (the
            // state was already `Cancelled`), so reap history here.
            self.evict_completed();
        }
        if forced {
            Ok(format!(
                "cancelled job {job_id} ({})",
                forced_abort_note(&unlocked)
            ))
        } else {
            Ok(format!("cancelled job {job_id}"))
        }
    }

    /// Releases the operation lock a force-aborted dispatch left behind, on
    /// every template in `rrids`.
    ///
    /// An empty `rrids` means the job recorded no scope (see [`Job::rrids`]) and
    /// falls back to every loaded template **plus the null sentinel**. The whole
    /// pass is bounded by `budget`, **including the preamble**: a template not
    /// reached in time is reported unknown rather than waited out, so
    /// `job_cancel` stays responsive. The sentinel is reserved one group's equal
    /// share of the budget so a blocked template cannot starve it. Per template
    /// it does nothing but release the group's own held locks — no disconnect,
    /// no pool release, no history row.
    async fn unlock_after_abort(&self, rrids: &[String], budget: Duration) -> AbortUnlock {
        let mut summary = AbortUnlock::default();
        // Armed before the first await: the preamble takes the canonical session
        // mutex, which an exclusive dispatch or a `get`/`put` transfer holds for
        // its whole duration, and leaving that outside the budget is exactly the
        // minutes-long block `ABORT_UNLOCK_BUDGET` promises to prevent.
        let deadline = Instant::now() + budget;

        // Preamble, deliberately **gate-free and unconditional**: drop the active
        // guard an aborted *exclusive* dispatch left on the canonical session,
        // then resolve the fallback scope. Doing it before, and independently of,
        // the per-template holds is load-bearing — that guard blocks
        // `Session::activate`'s `try_lock_owned`, so every later scoped dispatch
        // on the template would silently run against the null report, the
        // `list_locks` the reply recommends included. Dropped only inside the
        // per-template pass, a busy gate (RwGate is writer-preferring, so one
        // pending `load_template` blocks shared acquisition) could burn the
        // budget and leave the session poisoned.
        //
        // Gate-free is safe here, and `close_with_timeout` relies on the same
        // property: every writer of the canonical active guard either runs under
        // the exclusive gate or holds the session mutex for the whole write, so
        // taking the mutex makes the drop atomic against them.
        let preamble = async {
            let mut session = self.session.lock().await;
            session.release_active_guard();
            if rrids.is_empty() {
                // The registry alone is not the set of connected hosts: a host
                // attached with nothing loaded lives on the null sentinel, whose
                // RRID is empty and so is never returned by `rrids()` (#485).
                // Include it when it actually holds hosts, alongside the
                // registry's RRIDs.
                let with_null = session.null_group_has_hosts();
                (session.templates.rrids(), with_null)
            } else {
                // An explicit RRID scope names templates in the registry; the
                // null sentinel is not one of them, so it stays out (a scoped
                // dispatch could not have locked a sentinel host).
                (rrids.to_vec(), false)
            }
        };
        let Ok((targets, with_null)) = tokio::time::timeout(budget, preamble).await else {
            // Nothing is stranded by giving up here: the mutex being held that
            // long means a *live* dispatch owns it, and a live dispatch releases
            // its own active guard on the way out. The lingering-guard case (an
            // aborted exclusive job) leaves the mutex free, so this branch
            // cannot be that case.
            tracing::warn!(?budget, "post-abort unlock: session busy, release skipped");
            summary.stalled = true;
            return summary;
        };

        // One group's equal share of what the preamble left, reserved for the
        // sentinel; the templates share the rest. Un-reserved, one blocked
        // template (a wedged host, or a queued writer winning the
        // writer-preferring gate) eats the whole budget and the sentinel
        // release is never attempted.
        let null_reserve = if with_null {
            let groups = u32::try_from(targets.len().saturating_add(1)).unwrap_or(u32::MAX);
            deadline.saturating_duration_since(Instant::now()) / groups
        } else {
            Duration::ZERO
        };
        let templates_deadline = deadline - null_reserve;

        for rrid in targets {
            let left = templates_deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.unlock_template(&rrid)).await {
                Ok(outcomes) => summary.absorb(outcomes),
                Err(_) => {
                    tracing::warn!(rrid = %rrid, ?budget, "post-abort unlock timed out");
                    summary.unknown.push(rrid);
                }
            }
        }

        // The sentinel, on its reserved share plus whatever the templates left
        // over. Unlocks its own held locks only (`unlock_held`); a dispatch that
        // reached it was force-aborted, so leaving its `/var/lock/mtui.lock`
        // held would otherwise survive until the session ends (teardown) —
        // which under a long-lived MCP server may be never. On timeout the group
        // is reported as unknown rather than silently omitted.
        if with_null {
            let left = deadline.saturating_duration_since(Instant::now());
            let null_unlock = async {
                let mut session = self.session.lock().await;
                session.unlock_null_group_held().await
            };
            match tokio::time::timeout(left, null_unlock).await {
                Ok(outcomes) => summary.absorb(outcomes),
                Err(_) => {
                    tracing::warn!(?budget, "post-abort unlock of the null group timed out");
                    // The sentinel has no RRID, so it does not belong in
                    // `unknown` (whose remedy points at `list_locks -T <rrid>`);
                    // it gets its own clause with the bare-tool remedy.
                    summary.null_group_unknown = true;
                }
            }
        }
        summary
    }

    /// Releases one template's own held operation locks, taking the same holds a
    /// dispatch on it would.
    ///
    /// The gate-shared + per-RRID hold is what makes locking the report entry
    /// safe: [`Session::activate`] claims an entry with a *non-blocking*
    /// `try_lock_owned` and falls back to the null report when it fails, so a
    /// dispatch racing an entry lock this pass holds would silently act on
    /// nothing.
    ///
    /// They shut out a same-RRID dispatch and any exclusive one, which is all
    /// this pass needs, but they are not a crate-wide guarantee: `command_lock`
    /// resolves its scope and *then* acquires, and `close_with_timeout` locks
    /// entries gate-free.
    async fn unlock_template(&self, rrid: &str) -> BTreeMap<String, LockOutcome> {
        // Same acquire order as `command_lock` (gate-shared → one rrid lock), and
        // only ever one rrid lock at a time, so this cannot deadlock against a
        // concurrent dispatch.
        let _shared = self.gate.shared().await;
        let rrid_lock = self.lock_for(rrid);
        let _rrid = rrid_lock.lock_owned().await;

        // The preamble already dropped any guard the aborted dispatch left.
        let entry = self.session.lock().await.templates.handle(rrid);
        // Unloaded since the job was minted: nothing of ours to release.
        let Some(entry) = entry else {
            return BTreeMap::new();
        };
        // The scoped path's inner dispatch task is aborted asynchronously, so it
        // may still hold this entry briefly; the caller's budget bounds the wait.
        let mut report = entry.lock().await;
        report.base_mut().targets.unlock_held().await
    }

    /// Look up a job record by id, or the `"no such job"` envelope.
    fn job(&self, job_id: &str) -> Result<Arc<StdMutex<Job>>, McpCommandError> {
        self.jobs
            .lock()
            .expect("jobs table poisoned")
            .get(job_id)
            .cloned()
            .ok_or_else(|| McpCommandError {
                stdout: String::new(),
                stderr: format!("no such job: {job_id}"),
                exit_code: 1,
            })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use mtui_core::register_all;
    use mtui_hosts::{MockConnection, MockSftpOp, TARGET_LOCK_PATH};

    use super::*;

    fn session(config: Config) -> Arc<McpSession> {
        McpSession::new(config)
    }

    /// The freshness invariant `remint_after_drop_is_a_new_session` relies on
    /// instead of `Arc` address identity.
    #[test]
    fn session_id_is_unique_and_stable() {
        let a = McpSession::new(Config::default());
        let b = McpSession::new(Config::default());
        assert_ne!(a.id(), b.id(), "distinct sessions must have distinct ids");
        assert_eq!(a.id(), a.id(), "a session's id is stable across calls");
    }

    /// A host attached with **no template loaded** lives in the null-report
    /// group, which no registry walk sees; eviction must still reap it.
    #[tokio::test]
    async fn close_reaps_hosts_attached_with_no_template() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        let probe = MockConnection::new("n1");
        let mut target =
            Target::with_connection("n1", TargetState::Enabled, Box::new(probe.clone()));
        target.lock("").await.expect("operation lock taken");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_some(),
            "fixture must arm the assertion — the remote lock exists before close"
        );

        let sess = McpSession::new(Config::default());
        {
            let mut guard = sess.session().lock().await;
            assert!(
                !guard.metadata().is_loaded(),
                "fixture must reach the no-template state"
            );
            guard.targets_mut().add(target);
        }

        sess.close().await;
        assert!(probe.is_closed(), "n1: disconnected on eviction");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_none(),
            "n1: remote operation lock released on eviction"
        );

        let removes = || {
            probe
                .sftp_ops()
                .iter()
                .filter(|op| {
                    matches!(op, MockSftpOp::Remove(p) if p == &PathBuf::from(TARGET_LOCK_PATH))
                })
                .count()
        };
        assert_eq!(removes(), 1);
        sess.close().await;
        // Freshness is not observable here (a repeated `Target::close` skips
        // unlock either way); the seam test
        // `take_teardown_units_cover_registry_and_null_group_once` pins it.
        assert_eq!(removes(), 1, "no second lock-file removal");
    }

    /// A host whose `close()` never returns must not block `close_with_timeout`;
    /// the healthy sibling must still be closed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_with_timeout_survives_a_wedged_close() {
        use mtui_hosts::{HostsGroup, MockConnection, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let gate = Arc::new(tokio::sync::Notify::new());
        let wedged = MockConnection::new("wedged-host").with_blocking_close(Arc::clone(&gate));
        let good = MockConnection::new("good-host");
        let wedged_target =
            Target::with_connection("wedged-host", TargetState::Enabled, Box::new(wedged));
        let good_target =
            Target::with_connection("good-host", TargetState::Enabled, Box::new(good.clone()));

        let sess = McpSession::new(Config::default());
        {
            let mut guard = sess.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
            report.base_mut().targets = HostsGroup::new(vec![wedged_target, good_target], false);
            guard.templates.add(Box::new(report));
            guard.templates.set_active("SUSE:Maintenance:1:1");
        }

        // Generous outer guard: a regression that waited on the wedged close
        // fails loudly here instead of hanging the suite.
        let bounded = tokio::time::timeout(
            Duration::from_secs(15),
            sess.close_with_timeout(Duration::from_millis(200)),
        )
        .await;
        assert!(bounded.is_ok(), "close_with_timeout did not return in time");

        assert!(
            good.is_closed(),
            "healthy host closed despite wedged sibling"
        );

        // Release the abandoned close so its task unwinds.
        gate.notify_waiters();
    }

    /// Nor must a session mutex held for the whole duration, as an exclusive
    /// command holds it. The mutation this catches: without a timeout around the
    /// mutex-taking preamble, the close blocks as long as the holder does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_with_timeout_returns_when_session_mutex_is_busy() {
        let sess = McpSession::new(Config::default());

        let lock_acquired = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let holder = tokio::spawn({
            let session = Arc::clone(sess.session());
            let lock_acquired = Arc::clone(&lock_acquired);
            let release = Arc::clone(&release);
            async move {
                let _guard = session.lock().await;
                lock_acquired.notify_one();
                release.notified().await;
            }
        });
        lock_acquired.notified().await;

        // Generous outer guard, as above.
        let bounded = tokio::time::timeout(
            Duration::from_secs(15),
            sess.close_with_timeout(Duration::from_millis(200)),
        )
        .await;
        assert!(
            bounded.is_ok(),
            "close_with_timeout did not return while the session mutex was busy"
        );

        release.notify_one();
        holder.await.expect("holder task panicked");
    }

    /// The non-interactive contract: no prompter wired, and
    /// `interactive = false` from `capture::session`'s `is_repl = false`.
    #[tokio::test]
    async fn new_session_is_non_interactive() {
        let sess = session(Config::default());
        let guard = sess.session().lock().await;
        assert!(
            guard.prompter().is_none(),
            "MCP session must have no prompter"
        );
    }

    /// `whoami` returns the same banner the REPL prints, via the shared engine.
    #[tokio::test]
    async fn run_command_whoami_returns_stdout() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("whoami succeeds");
        assert!(out.starts_with("User: testuser, app pid: "), "got: {out:?}");
        assert!(out.ends_with('\n'), "trailing newline preserved: {out:?}");
    }

    /// An unknown flag is exit 2, with the offending token in stderr.
    #[tokio::test]
    async fn run_command_argparse_failure_raises() {
        let sess = session(Config::default());
        let registry = register_all();

        let err = sess
            .run_command(&registry, "whoami", &["--bogus".to_owned()])
            .await
            .expect_err("unknown flag must fail");
        assert_eq!(err.exit_code, 2, "parse errors are argparse-exit-2");
        assert!(
            err.stderr.contains("bogus") || err.to_string().contains("bogus"),
            "stderr should name the bad flag: {err:?}"
        );
    }

    /// An unknown command is exit 1, not a parse error.
    #[tokio::test]
    async fn run_command_unknown_command_raises_exit_1() {
        let sess = session(Config::default());
        let registry = register_all();

        let err = sess
            .run_command(&registry, "no_such_command", &[])
            .await
            .expect_err("unknown command must fail");
        assert_eq!(err.exit_code, 1);
    }

    /// `--help` is argparse-exit-0: help text as a success, not an envelope.
    #[tokio::test]
    async fn run_command_help_flag_is_success() {
        let sess = session(Config::default());
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &["--help".to_owned()])
            .await
            .expect("--help is a success");
        assert!(!out.is_empty(), "help text returned: {out:?}");
    }

    /// A tiny cap truncates the result and appends the notice.
    #[tokio::test]
    async fn run_command_output_is_capped() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_max_output_bytes = 8; // far below the `whoami` banner length
        let sess = session(config);
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("whoami succeeds");
        assert!(out.contains("truncated"), "cap notice present: {out:?}");
        assert!(
            out.contains("max_output_bytes=8"),
            "cap limit in notice: {out:?}"
        );
    }

    /// Bounding happens at *write time*: one notice with the correct overrun
    /// count proves the payload was discarded as written, not buffered and
    /// trimmed.
    #[tokio::test]
    async fn run_command_bounds_giant_output_at_write_time() {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        /// Emits `n` 'x' bytes to the display in one write.
        struct Flood(usize);
        #[async_trait::async_trait]
        impl Command for Flood {
            fn name(&self) -> &'static str {
                "flood_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Fanout
            }
            async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
                session.display.println(&"x".repeat(self.0));
                Ok(())
            }
        }

        let cap = 16;
        let flood = 10_000;
        // `println` appends a trailing newline, so the display emits flood + 1 bytes.
        let total = flood + 1;
        let mut config = Config::default();
        config.mcp_max_output_bytes = cap;
        let sess = session(config);
        let mut registry = register_all();
        registry.register(Arc::new(Flood(flood)));

        let out = sess
            .run_command(&registry, "flood_probe", &[])
            .await
            .expect("flood succeeds");
        assert!(
            out.starts_with(&"x".repeat(cap)),
            "head kept: {}",
            &out[..40]
        );
        assert_eq!(out.matches("truncated").count(), 1, "exactly one notice");
        assert!(
            out.contains(&format!("truncated {} bytes", total - cap)),
            "correct dropped count: {out}"
        );
        assert!(out.contains(&format!("max_output_bytes={cap}")));
    }

    /// A second call must not see the first call's captured text.
    #[tokio::test]
    async fn run_command_isolates_output_per_call() {
        let mut config = Config::default();
        config.session_user = "alice".to_owned();
        let sess = session(config);
        let registry = register_all();

        let first = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        let second = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        // Identical single-banner output, not the first call's text doubled.
        assert_eq!(first, second);
        assert_eq!(
            second.matches("User: alice").count(),
            1,
            "no bleed: {second:?}"
        );
    }

    #[test]
    fn command_error_display() {
        let with_stderr = McpCommandError {
            stdout: String::new(),
            stderr: "unrecognized argument: --bogus".to_owned(),
            exit_code: 2,
        };
        assert_eq!(
            with_stderr.to_string(),
            "command failed (exit_code=2): unrecognized argument: --bogus"
        );

        let no_stderr = McpCommandError {
            stdout: String::new(),
            stderr: "   ".to_owned(),
            exit_code: 1,
        };
        assert_eq!(no_stderr.to_string(), "command failed (exit_code=1)");
    }

    /// Eviction FIFO-drops the oldest terminal records and never a running one.
    /// Driven against the private jobs table with fabricated records: an
    /// integration test cannot force a concurrent completion while another job
    /// holds the single session mutex.
    #[tokio::test]
    async fn evict_completed_fifo_and_spares_running() {
        let mut config = Config::default();
        config.mcp_max_completed_jobs = 2;
        let sess = session(config);

        let base = Instant::now();
        let mk = |id: &str, state: JobState, finished: Option<Instant>| {
            Arc::new(StdMutex::new(Job {
                id: id.to_owned(),
                command: "probe".to_owned(),
                rrids: Vec::new(),
                state,
                started: base,
                finished,
                result: None,
                error: None,
                exit_code: None,
                handle: None,
                cancel: CancellationToken::new(),
            }))
        };
        {
            let mut jobs = sess.jobs.lock().unwrap();
            jobs.insert("t-1".to_owned(), mk("t-1", JobState::Done, Some(base)));
            jobs.insert(
                "t-2".to_owned(),
                mk("t-2", JobState::Failed, Some(base + Duration::from_secs(1))),
            );
            jobs.insert(
                "t-3".to_owned(),
                mk(
                    "t-3",
                    JobState::Cancelled,
                    Some(base + Duration::from_secs(2)),
                ),
            );
            jobs.insert("run".to_owned(), mk("run", JobState::Running, None));
        }

        sess.evict_completed();

        let ids: std::collections::HashSet<String> =
            sess.job_list().into_iter().map(|j| j.id).collect();
        assert!(!ids.contains("t-1"), "oldest terminal evicted: {ids:?}");
        assert!(ids.contains("t-2"), "kept: {ids:?}");
        assert!(ids.contains("t-3"), "kept: {ids:?}");
        assert!(ids.contains("run"), "running never evicted: {ids:?}");
        assert_eq!(ids.len(), 3);
    }

    /// A zero completed cap disables eviction.
    #[tokio::test]
    async fn evict_completed_zero_cap_is_disabled() {
        let mut config = Config::default();
        config.mcp_max_completed_jobs = 0;
        let sess = session(config);
        {
            let mut jobs = sess.jobs.lock().unwrap();
            for i in 0..5 {
                jobs.insert(
                    format!("t-{i}"),
                    Arc::new(StdMutex::new(Job {
                        id: format!("t-{i}"),
                        command: "probe".to_owned(),
                        rrids: Vec::new(),
                        state: JobState::Done,
                        started: Instant::now(),
                        finished: Some(Instant::now()),
                        result: None,
                        error: None,
                        exit_code: None,
                        handle: None,
                        cancel: CancellationToken::new(),
                    })),
                );
            }
        }
        sess.evict_completed();
        assert_eq!(sess.job_list().len(), 5, "zero cap keeps everything");
    }

    /// One lock object per RRID, so same-RRID calls contend and others do not.
    #[test]
    fn lock_for_shares_per_rrid() {
        let sess = session(Config::default());
        let a1 = sess.lock_for("SUSE:Maintenance:1:1");
        let a2 = sess.lock_for("SUSE:Maintenance:1:1");
        let b = sess.lock_for("SUSE:Maintenance:2:1");
        assert!(Arc::ptr_eq(&a1, &a2), "same RRID shares one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "distinct RRIDs get distinct locks");
    }

    /// No resolvable RRID → the gate is taken exclusively.
    #[tokio::test]
    async fn command_lock_unknown_is_exclusive() {
        let sess = session(Config::default());
        let registry = register_all();
        let lock = sess.command_lock(&registry, "no_such_command", &[]).await;
        assert!(
            matches!(lock, CommandLock::Exclusive(_)),
            "unknown command serialises exclusively"
        );
    }

    #[tokio::test]
    async fn command_lock_unscoped_is_exclusive() {
        let sess = session(Config::default());
        let registry = register_all();
        // `whoami` is `Scope::Active`; with nothing loaded it resolves to the
        // empty null RRID, which `resolve_command_rrids` drops → exclusive.
        let lock = sess.command_lock(&registry, "whoami", &[]).await;
        assert!(matches!(lock, CommandLock::Exclusive(_)));
    }

    /// With nothing loaded, `scoped_lock(None)` falls back to the empty active
    /// RRID and still yields a scoped hold rather than deadlocking.
    #[tokio::test]
    async fn scoped_lock_falls_back_to_active() {
        let sess = session(Config::default());
        let lock = sess.scoped_lock(None).await;
        assert!(matches!(lock, CommandLock::Scoped { .. }));
    }

    /// A registry mutator takes the gate *exclusive* even when one template is
    /// loaded and `resolve_command_rrids` would give one RRID, because a
    /// structural mutation must land on the canonical session, not a discarded
    /// fork. A content command on that template still takes the scoped path.
    #[tokio::test]
    async fn command_lock_registry_mutator_is_exclusive_even_when_scoped() {
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;

        let sess = session(Config::default());
        let rrid = "SUSE:Maintenance:1:1";
        {
            let mut guard = sess.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
            guard.templates.add(Box::new(report));
            guard.templates.set_active(rrid);
        }
        let registry = register_all();

        let mutator = sess
            .command_lock(&registry, "load_template", &[rrid.to_owned()])
            .await;
        assert!(
            matches!(mutator, CommandLock::Exclusive(_)),
            "registry mutator must take the exclusive gate"
        );
        drop(mutator);

        let scoped = sess
            .command_lock(&registry, "list_hosts", &["-T".to_owned(), rrid.to_owned()])
            .await;
        assert!(
            matches!(scoped, CommandLock::Scoped { .. }),
            "content command on one template stays on the scoped path"
        );
    }

    /// Cancelling a *finished* job succeeds as a no-op and does not rewrite it.
    #[tokio::test]
    async fn job_cancel_finished_job_is_noop() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let registry = Arc::new(register_all());

        let job_id = sess
            .start_job(Arc::clone(&registry), "whoami", Vec::new())
            .expect("start_job succeeds");
        for _ in 0..500 {
            if sess.job_status(&job_id).unwrap().state != JobState::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Done);

        let msg = sess.job_cancel(&job_id).await.expect("cancel is a no-op");
        assert_eq!(msg, format!("job {job_id} already done; nothing to cancel"));
        // Not rewritten to Cancelled, and the result survives.
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Done);
        assert!(
            sess.job_result(&job_id).is_ok(),
            "result survives the no-op"
        );
    }

    /// A body watching the cancellation seam settles inside the grace window:
    /// `job_cancel` reports a plain (cooperative) cancel, not a forced abort.
    #[tokio::test]
    async fn job_cancel_cooperative_body_settles_within_grace() {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        // Signals once the body is parked on the seam, so the cancel is issued
        // only after the dispatch is genuinely mid-flight.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        struct Cooperative(StdMutex<Option<tokio::sync::oneshot::Sender<()>>>);
        #[async_trait::async_trait]
        impl Command for Cooperative {
            fn name(&self) -> &'static str {
                "cooperative_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Fanout
            }
            async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
                if let Some(tx) = self.0.lock().expect("probe channel poisoned").take() {
                    let _ = tx.send(());
                }
                // Unwinds the moment job_cancel fires, inside CANCEL_GRACE.
                session.cancel_token().cancelled().await;
                Err(CommandError::Cancelled(String::new()))
            }
        }

        let sess = session(Config::default());
        let mut registry = register_all();
        registry.register(Arc::new(Cooperative(StdMutex::new(Some(started_tx)))));

        let job_id = sess
            .start_job(Arc::new(registry), "cooperative_probe", Vec::new())
            .expect("start_job succeeds");
        started_rx.await.expect("probe body started");

        let before = Instant::now();
        let msg = sess.job_cancel(&job_id).await.expect("cancel succeeds");
        // The body observed the token, so no forced abort, and the whole cancel
        // settles well inside the grace window.
        assert_eq!(msg, format!("cancelled job {job_id}"));
        assert!(
            before.elapsed() < CANCEL_GRACE,
            "cooperative cancel must not burn the grace: {:?}",
            before.elapsed()
        );
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Cancelled);
    }

    /// A body that never checks the seam burns the full grace and is then
    /// force-aborted; the reply says so instead of claiming a clean cancel.
    #[tokio::test]
    async fn job_cancel_unobservant_body_is_force_aborted_after_grace() {
        use clap::ArgMatches;
        use mtui_core::{Command, CommandResult, Scope};

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        struct Stubborn(StdMutex<Option<tokio::sync::oneshot::Sender<()>>>);
        #[async_trait::async_trait]
        impl Command for Stubborn {
            fn name(&self) -> &'static str {
                "stubborn_probe"
            }
            fn scope(&self) -> Scope {
                Scope::Fanout
            }
            async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
                if let Some(tx) = self.0.lock().expect("probe channel poisoned").take() {
                    let _ = tx.send(());
                }
                // Simulates a body blocked mid host-op: never observes the
                // token, only the hard abort can stop it.
                tokio::time::sleep(Duration::from_secs(600)).await;
                Ok(())
            }
        }

        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let mut registry = register_all();
        registry.register(Arc::new(Stubborn(StdMutex::new(Some(started_tx)))));
        let registry = Arc::new(registry);

        let job_id = sess
            .start_job(Arc::clone(&registry), "stubborn_probe", Vec::new())
            .expect("start_job succeeds");
        started_rx.await.expect("probe body started");

        let msg = sess.job_cancel(&job_id).await.expect("cancel succeeds");
        assert!(
            msg.contains("forced abort"),
            "unobservant body must report the forced abort: {msg}"
        );
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Cancelled);
        let err = sess.job_result(&job_id).expect_err("cancelled job raises");
        assert!(err.stderr.contains("was cancelled"), "got: {err:?}");

        // Self-healing end-to-end: the hard abort skipped the exclusive path's
        // token restore, so the next dispatch must install a fresh token before
        // its pre-flight check rather than inherit the cancelled one.
        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("dispatch after a forced abort must not see a stale cancelled token");
        assert!(out.contains("testuser"), "got: {out}");
    }

    /// The no-op reply for the two other terminal states names each actual state.
    #[tokio::test]
    async fn job_cancel_failed_and_cancelled_jobs_reply_truthfully() {
        let sess = session(Config::default());
        for (id, state) in [
            ("probe-failed-1", JobState::Failed),
            ("probe-cancelled-1", JobState::Cancelled),
        ] {
            let job = Arc::new(StdMutex::new(Job {
                id: id.to_owned(),
                command: "probe".to_owned(),
                rrids: Vec::new(),
                state,
                started: Instant::now(),
                finished: Some(Instant::now()),
                result: None,
                error: None,
                exit_code: None,
                handle: None,
                cancel: CancellationToken::new(),
            }));
            sess.jobs.lock().unwrap().insert(id.to_owned(), job);

            let msg = sess.job_cancel(id).await.expect("terminal cancel is Ok");
            assert_eq!(msg, format!("job {id} already {state}; nothing to cancel"));
            assert_eq!(sess.job_status(id).unwrap().state, state);
        }
    }

    #[tokio::test]
    async fn job_result_cancelled_job_raises() {
        let sess = session(Config::default());
        let job = Arc::new(StdMutex::new(Job {
            id: "whoami-1".to_owned(),
            command: "whoami".to_owned(),
            rrids: Vec::new(),
            state: JobState::Cancelled,
            started: Instant::now(),
            finished: Some(Instant::now()),
            result: None,
            error: None,
            exit_code: None,
            handle: None,
            cancel: CancellationToken::new(),
        }));
        sess.jobs.lock().unwrap().insert("whoami-1".to_owned(), job);

        let err = sess
            .job_result("whoami-1")
            .expect_err("cancelled job raises on job_result");
        assert!(err.stderr.contains("was cancelled"), "got: {err:?}");
        assert_eq!(err.exit_code, 1);
    }

    // ---- forced cancel: the stranded operation lock (#405) ------------------ //

    const LOCK_RRID_A: &str = "SUSE:Maintenance:1:1";
    const LOCK_RRID_B: &str = "SUSE:Maintenance:2:1";

    /// Loads `rrid` with a host group over `mocks` into `sess` and makes it
    /// active. The mock handles stay `Arc`-shared with the caller's, so
    /// lock/unlock SFTP ops remain observable after the targets move into the
    /// group.
    async fn load_with_hosts(sess: &McpSession, rrid: &str, mocks: &[(&str, MockConnection)]) {
        use mtui_hosts::{HostsGroup, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let targets: Vec<Target> = mocks
            .iter()
            .map(|(name, mock)| {
                Target::with_connection(*name, TargetState::Enabled, Box::new(mock.clone()))
            })
            .collect();
        let mut guard = sess.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        report.base_mut().targets = HostsGroup::new(targets, false);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(rrid);
    }

    /// Blocks until `mock` records the exclusive lockfile create. Anti-vacuity
    /// guard: without it a cancel could land before the dispatch ever locked, and
    /// "the lock was released" would pass against a never-locked host.
    async fn await_locked(mock: &MockConnection, who: &str) {
        for _ in 0..2000 {
            if mock.sftp_ops().iter().any(|op| {
                matches!(op, MockSftpOp::Write { path, exclusive: true }
                    if path == &PathBuf::from(TARGET_LOCK_PATH))
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{who} never took the operation lock");
    }

    /// Whether `mock` recorded a lockfile removal.
    fn saw_unlock(mock: &MockConnection) -> bool {
        mock.sftp_ops()
            .iter()
            .any(|op| matches!(op, MockSftpOp::Remove(p) if p == &PathBuf::from(TARGET_LOCK_PATH)))
    }

    /// Blocks until `mock` records a **non-exclusive** lockfile write — the
    /// re-stamp `lock()` performs over a lock this process already holds. The
    /// anti-vacuity anchor when the host was locked before the job started, where
    /// `await_locked` would be satisfied by the *earlier* exclusive create.
    async fn await_relocked(mock: &MockConnection, who: &str) {
        for _ in 0..2000 {
            if mock.sftp_ops().iter().any(|op| {
                matches!(op, MockSftpOp::Write { path, exclusive: false }
                    if path == &PathBuf::from(TARGET_LOCK_PATH))
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{who} never re-stamped the operation lock");
    }

    /// Whether the lockfile is still on `mock`'s (simulated) filesystem.
    fn still_locked(mock: &MockConnection) -> bool {
        mock.file_contents(TARGET_LOCK_PATH).is_some()
    }

    /// A lockfile line stamped with **this process's** identity: what a sibling
    /// template's (or another MCP session's) live hold looks like on a shared
    /// refhost, since wire ownership is per user + PID and so reads back as
    /// "mine" to every host group in this process. `Target::with_connection`
    /// builds its lock from `Config::default()`, so that is the identity to match.
    fn ours_lockfile() -> Vec<u8> {
        format!(
            "1700000000:{}:{}",
            Config::default().session_user,
            std::process::id()
        )
        .into_bytes()
    }

    /// A fan-out probe that takes the group's operation lock and then parks:
    /// [`Park::Forever`] never observes the cancellation seam (so the cancel must
    /// force-abort it, stranding the lock — the #405 shape), [`Park::Seam`]
    /// unwinds cooperatively, and [`Park::Gate`] waits for a test-issued permit
    /// then runs its own `unlock()` (a well-behaved concurrent job).
    ///
    /// The gate is a [`Semaphore`](tokio::sync::Semaphore), not a `Notify`,
    /// because a permit is *stored*: a release issued before the body reaches the
    /// await is still observed, where `notify_waiters` would be lost and hang.
    enum Park {
        Forever,
        Seam,
        Gate(Arc<tokio::sync::Semaphore>),
    }

    struct LockAndPark {
        park: Park,
        /// Park only while acting for this template; every other template locks
        /// and returns at once. `None` parks on the first template it reaches —
        /// which, on a fan-out, means later templates never run at all.
        park_rrid: Option<String>,
    }

    impl LockAndPark {
        /// Locks the whole group with no comment (an ordinary operation hold).
        fn new(park: Park) -> Self {
            Self {
                park,
                park_rrid: None,
            }
        }

        /// As [`new`](Self::new) but parks only for `rrid`, so a fan-out gets
        /// through every template and each ends up holding its own lock.
        fn parking_on(park: Park, rrid: &str) -> Self {
            Self {
                park,
                park_rrid: Some(rrid.to_owned()),
            }
        }
    }

    #[async_trait::async_trait]
    impl mtui_core::Command for LockAndPark {
        fn name(&self) -> &'static str {
            "lock_and_park_probe"
        }
        fn scope(&self) -> mtui_core::Scope {
            mtui_core::Scope::Fanout
        }
        async fn call(
            &self,
            session: &mut Session,
            _args: &clap::ArgMatches,
        ) -> mtui_core::CommandResult {
            let rrid = session.metadata().id();
            session.targets_mut().lock("").await;
            if self.park_rrid.as_ref().is_some_and(|r| *r != rrid) {
                return Ok(());
            }
            match &self.park {
                Park::Forever => {
                    // Blocked mid host-op: never reaches the seam, so only the
                    // hard abort stops it and its `unlock()` never runs.
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
                Park::Seam => {
                    session.cancel_token().cancelled().await;
                    return Err(CommandError::Cancelled(String::new()));
                }
                Park::Gate(gate) => {
                    let _permit = gate.acquire().await.expect("gate not closed");
                    session.targets_mut().unlock().await;
                }
            }
            Ok(())
        }
    }

    fn registry_with_probe(probe: LockAndPark) -> Arc<Registry> {
        let mut registry = register_all();
        registry.register(Arc::new(probe));
        Arc::new(registry)
    }

    /// `scope_argv` pins an unscoped argv and leaves an already-scoped one
    /// alone — including `--all-templates`, which *conflicts with* `-T` and
    /// would turn the dispatch into a parse error rather than scoping it.
    #[test]
    fn scope_argv_pins_only_an_unscoped_argv() {
        assert_eq!(
            scope_argv("R:1", &["true".to_owned()]),
            vec!["-T".to_owned(), "R:1".to_owned(), "true".to_owned()],
            "prepended, so a REMAINDER positional cannot swallow it"
        );
        for already in [
            vec!["-T".to_owned(), "R:2".to_owned()],
            vec!["--template".to_owned(), "R:2".to_owned()],
            vec!["--all-templates".to_owned()],
        ] {
            assert_eq!(
                scope_argv("R:1", &already),
                already,
                "an explicit scope flag must be left alone"
            );
        }
    }

    /// A probe that records the `-T` value its dispatch was given.
    struct RecordTemplate(Arc<StdMutex<Vec<Option<String>>>>);

    #[async_trait::async_trait]
    impl mtui_core::Command for RecordTemplate {
        fn name(&self) -> &'static str {
            "record_template_probe"
        }
        fn scope(&self) -> mtui_core::Scope {
            mtui_core::Scope::Fanout
        }
        async fn call(
            &self,
            _session: &mut Session,
            args: &clap::ArgMatches,
        ) -> mtui_core::CommandResult {
            self.0
                .lock()
                .unwrap()
                .push(args.get_one::<String>("template").cloned());
            Ok(())
        }
    }

    /// A backgrounded single-template job dispatches **pinned** to the template
    /// its record names: resolution happens at mint and dispatch later, so an
    /// unpinned argv lets a `load_template` in between widen the dispatch beyond
    /// the record, and a cancel would then release only the recorded template's
    /// locks while reporting success.
    #[tokio::test]
    async fn start_jobs_pins_the_single_template_dispatch_to_its_record() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[]).await;
        let mut registry = register_all();
        registry.register(Arc::new(RecordTemplate(Arc::clone(&seen))));

        let ids = sess
            .start_jobs(Arc::new(registry), "record_template_probe", Vec::new())
            .await
            .expect("start_jobs succeeds");
        assert_eq!(ids.len(), 1);
        for _ in 0..2000 {
            if sess.job_status(&ids[0]).expect("job exists").state != JobState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(sess.job_status(&ids[0]).unwrap().state, JobState::Done);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some(LOCK_RRID_A.to_owned())],
            "the dispatch must carry the template the job recorded"
        );
    }

    /// #405 headline: a job force-aborted mid host-operation has its lock
    /// released on every host of the template it was scoped to, and the reply
    /// names them. Driven through the real `run`, where the abort genuinely
    /// lands between `lock_selected` and `unlock_selected`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_releases_the_stranded_operation_lock() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        let beta = MockConnection::new("host-beta").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[("host-alpha", alpha.clone()), ("host-beta", beta.clone())],
        )
        .await;
        let registry = Arc::new(register_all());

        let ids = sess
            .start_jobs(Arc::clone(&registry), "run", vec!["true".to_owned()])
            .await
            .expect("start_jobs succeeds");
        assert_eq!(ids.len(), 1, "one template -> one job: {ids:?}");
        await_locked(&alpha, "host-alpha").await;
        await_locked(&beta, "host-beta").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            msg.contains("unlocked: host-alpha, host-beta"),
            "the reply must name the hosts it unlocked: {msg}"
        );
        assert!(saw_unlock(&alpha), "host-alpha's lock was never removed");
        assert!(saw_unlock(&beta), "host-beta's lock was never removed");
        assert!(!still_locked(&alpha), "host-alpha is still locked");
        assert!(!still_locked(&beta), "host-beta is still locked");
    }

    /// The exclusive dispatch path: an aborted unscoped fan-out leaves the
    /// canonical session holding the active entry's guard, so the release must
    /// drop it first — otherwise `job_cancel` deadlocks on the entry, and a
    /// later scoped dispatch silently falls back to the null report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_on_the_exclusive_path_unlocks_and_clears_the_active_guard() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));

        // `start_job` records no scope, and an unscoped fan-out over two
        // templates takes the gate exclusively — the inline dispatch path.
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        // The unfixed deadlock would hang here forever, not merely fail.
        let msg = tokio::time::timeout(Duration::from_secs(20), sess.job_cancel(&job_id))
            .await
            .expect("job_cancel must not deadlock on the lingering active guard")
            .expect("cancel succeeds");
        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(saw_unlock(&alpha), "host-alpha's lock was never removed");
        assert!(!still_locked(&alpha), "host-alpha is still locked");

        // No active guard may remain: a scoped dispatch claims the entry with a
        // *non-blocking* `try_lock_owned`, so a lingering one would not error —
        // it would silently list the null report's empty host set.
        let out = sess
            .run_command(
                &registry,
                "list_hosts",
                &["-T".to_owned(), LOCK_RRID_A.to_owned()],
            )
            .await
            .expect("list_hosts after a forced abort succeeds");
        assert!(
            out.contains("host-alpha"),
            "a lingering active guard sent the dispatch to the null report: {out:?}"
        );
    }

    /// The release is scoped to the cancelled job's own templates: a sibling job
    /// mid-operation elsewhere keeps its locks, and the cancel does not queue
    /// behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forced_cancel_leaves_a_concurrent_templates_locks_alone() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        // Both bodies lock and park; B is never released before the cancel.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let registry = registry_with_probe(LockAndPark::new(Park::Gate(Arc::clone(&gate))));

        let ids = sess
            .start_jobs(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .await
            .expect("start_jobs succeeds");
        assert_eq!(ids.len(), 2, "two templates -> two jobs: {ids:?}");
        let job_a = ids
            .iter()
            .find(|j| j.contains("SUSE_Maintenance_1_1"))
            .expect("job for A")
            .clone();
        let job_b = ids
            .iter()
            .find(|j| j.contains("SUSE_Maintenance_2_1"))
            .expect("job for B")
            .clone();
        await_locked(&alpha, "host-alpha").await;
        await_locked(&beta, "host-beta").await;

        let before = Instant::now();
        let msg = sess.job_cancel(&job_a).await.expect("cancel succeeds");
        let elapsed = before.elapsed();

        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        assert!(
            !msg.contains("host-beta"),
            "the reply must not mention another template's hosts: {msg}"
        );
        // The timing-independent pin: no bucket may name the other template.
        assert!(
            !msg.contains(LOCK_RRID_B),
            "the reply must not mention another template at all: {msg}"
        );
        assert!(
            !saw_unlock(&beta),
            "the cancel released a lock belonging to a live job on another template"
        );
        // Falling back to every loaded template would queue behind job B's
        // per-RRID lock and burn the whole budget.
        assert!(
            elapsed < CANCEL_GRACE + Duration::from_secs(3),
            "cancel waited on the other template's lock: {elapsed:?}"
        );

        // Job B held its lock the whole time; released, it finishes normally and
        // runs its own unlock.
        gate.add_permits(1);
        for _ in 0..2000 {
            if sess.job_status(&job_b).expect("job B exists").state != JobState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(sess.job_status(&job_b).unwrap().state, JobState::Done);
        assert!(saw_unlock(&beta), "job B never released its own lock");
    }

    /// All four host verdicts in one reply, each claiming only what happened.
    ///
    /// * `host-ok` — released.
    /// * `host-stolen` — this group *did* take the lock, but by release time the
    ///   remote line belongs to someone else (a reboot cleared `/var/lock`, a
    ///   stale reap fired): benign contention.
    /// * `host-broken1`/`2` — our lock, removal fails. Two of them, so a
    ///   "report only the first failure" bug cannot survive.
    /// * `host-foreign` — locked by someone else all along, so this group never
    ///   held it: absent from the verdict entirely, not reported as anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_reply_distinguishes_every_host_verdict() {
        let ok = MockConnection::new("host-ok");
        let stolen = MockConnection::new("host-stolen");
        let foreign = MockConnection::new("host-foreign").with_file(
            TARGET_LOCK_PATH,
            b"1700000000:someone-else:2147483647".to_vec(),
        );
        let broken1 = MockConnection::new("host-broken1").failing_sftp_remove();
        let broken2 = MockConnection::new("host-broken2").failing_sftp_remove();

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[
                ("host-ok", ok.clone()),
                ("host-stolen", stolen.clone()),
                ("host-foreign", foreign.clone()),
                ("host-broken1", broken1.clone()),
                ("host-broken2", broken2.clone()),
            ],
        )
        .await;

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&ok, "host-ok").await;
        await_locked(&stolen, "host-stolen").await;
        await_locked(&broken1, "host-broken1").await;
        await_locked(&broken2, "host-broken2").await;

        // `with_file` writes through the mock's `Arc`-shared file table, so this
        // lands on the clone the host group owns.
        let _ = stolen
            .clone()
            .with_file(TARGET_LOCK_PATH, b"1700000000:someone-else:4242".to_vec());

        let msg = sess.job_cancel(&job_id).await.expect("cancel succeeds");
        assert!(
            msg.contains("unlocked: host-ok)") || msg.contains("unlocked: host-ok;"),
            "only the released host may be listed as unlocked: {msg}"
        );
        assert!(
            msg.contains("still locked by another owner: host-stolen"),
            "got: {msg}"
        );
        assert!(msg.contains("unlock FAILED on host-broken1"), "got: {msg}");
        assert!(
            msg.contains("unlock FAILED on host-broken2"),
            "every failed host must be named, not just the first: {msg}"
        );
        assert!(
            msg.contains("unlock --force"),
            "the failure arm must name the remedy: {msg}"
        );
        assert!(
            !msg.contains("host-foreign"),
            "a lock this group never took must not be reported at all: {msg}"
        );
        // Truthful: every non-released host is in fact still locked.
        assert!(still_locked(&stolen), "the stolen lock was removed");
        assert!(still_locked(&foreign), "the foreign lock was removed");
        assert!(!saw_unlock(&foreign), "the foreign lock was acted on");
        assert!(still_locked(&broken1), "the failed removal claimed success");
        assert!(still_locked(&broken2), "the failed removal claimed success");
    }

    /// A host whose lock read outruns the budget is reported unknown, with the
    /// scoped remedy, rather than blocking the cancel or claiming the lock is
    /// gone. The budget sits **above** the preamble/gate/entry acquisition cost
    /// and **below** the mock's per-SFTP-read delay, so the expiry is
    /// attributable to the host fan-out and not to lock-acquisition noise.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_unlock_is_bounded_and_says_so_on_expiry() {
        // Every SFTP read costs 400ms; the release below gets 200ms.
        let slow = MockConnection::new("host-slow")
            .with_sftp_session_delay(Duration::from_millis(400))
            .with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-slow", slow.clone())]).await;
        let registry = Arc::new(register_all());

        let ids = sess
            .start_jobs(Arc::clone(&registry), "run", vec!["true".to_owned()])
            .await
            .expect("start_jobs succeeds");
        await_locked(&slow, "host-slow").await;

        let budget = Duration::from_millis(200);
        let before = Instant::now();
        let msg = sess
            .job_cancel_with_budget(&ids[0], budget)
            .await
            .expect("cancel succeeds");
        let elapsed = before.elapsed();

        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            msg.contains(&format!("lock state unknown on {LOCK_RRID_A}")),
            "an expired release must not be reported as done: {msg}"
        );
        assert!(msg.contains("list_locks -T <rrid>"), "got: {msg}");
        assert!(!msg.contains("unlocked:"), "nothing was unlocked: {msg}");
        // Grace + budget + slack: loose enough not to flake, tight enough that
        // waiting out even one 400ms host read would breach it.
        assert!(
            elapsed < CANCEL_GRACE + budget + Duration::from_millis(700),
            "the release was not bounded by the budget: {elapsed:?}"
        );
        // And the reply told the truth: the lock really is still there.
        assert!(!saw_unlock(&slow), "the lockfile removal was reached");
        assert!(still_locked(&slow), "host-slow's lock is gone after all");
    }

    /// The release belongs to the *forced* arm only. The probe deliberately
    /// unwinds through the seam **without** unlocking, so the surviving lock
    /// proves the abort-path release did not run — the pin is "forced-only", not
    /// "the cooperative body cleaned up" (a real cooperative flow owns its own
    /// unlock discipline). The reply must also stay byte-identical.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooperative_cancel_reply_and_locks_are_untouched() {
        let alpha = MockConnection::new("host-alpha");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;

        let registry = registry_with_probe(LockAndPark::new(Park::Seam));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&job_id).await.expect("cancel succeeds");
        assert_eq!(
            msg,
            format!("cancelled job {job_id}"),
            "the cooperative reply is a pinned contract"
        );
        assert!(
            !saw_unlock(&alpha),
            "the cooperative arm must not run the abort-path release"
        );
        assert!(still_locked(&alpha));
    }

    // ---- ownership scoping: only locks this job's own group took (#405) ----- //

    /// A same-process lock this group never took is left alone: on a refhost
    /// shared with another loaded template (or another MCP session), wire
    /// ownership is per-PID, so the sibling's live hold reads back as "mine" and
    /// a whole-group `unlock()` would strip it mid-transaction and report it
    /// released. Scoping on what this group's own `Target`s took separates them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_leaves_a_same_process_lock_this_group_never_took() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        // Seeded with *our own* user+pid: what a sibling template's live job
        // leaves on a shared refhost.
        let shared = MockConnection::new("shared-host")
            .with_file(TARGET_LOCK_PATH, ours_lockfile())
            .with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[
                ("host-alpha", alpha.clone()),
                ("shared-host", shared.clone()),
            ],
        )
        .await;
        let registry = Arc::new(register_all());

        // `run -t host-alpha`: this job locks only host-alpha.
        let ids = sess
            .start_jobs(
                Arc::clone(&registry),
                "run",
                vec!["-t".to_owned(), "host-alpha".to_owned(), "true".to_owned()],
            )
            .await
            .expect("start_jobs succeeds");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        assert!(
            !msg.contains("shared-host"),
            "the reply must not mention a host this job never locked: {msg}"
        );
        assert!(
            !saw_unlock(&shared),
            "the cancel removed a live lock this group never took"
        );
        assert!(still_locked(&shared), "the sibling's lock is gone");
    }

    /// A force-aborted job that locked a host attached with **no report loaded**
    /// (#485).
    ///
    /// The fallback scope (`Job::rrids` empty) used to resolve to
    /// `templates.rrids()` — the registry only — so the null sentinel, whose
    /// RRID is empty, was never reached and the lock it stranded survived until
    /// the session ended. The fix makes the fallback include the sentinel when
    /// it holds hosts. The host is locked by the *job* (not pre-locked), so the
    /// sentinel's own `Target` records the hold and `unlock_held` can release it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_releases_a_lock_on_a_host_with_no_report_loaded() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        let null_host = MockConnection::new("null-host");
        let target = Target::with_connection(
            "null-host",
            TargetState::Enabled,
            Box::new(null_host.clone()),
        );

        let sess = session(Config::default());
        {
            let mut guard = sess.session().lock().await;
            assert!(
                !guard.metadata().is_loaded(),
                "fixture must reach the no-template state"
            );
            guard.targets_mut().add(target);
        }

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));
        // `start_job` records no template scope, so the cancel falls back to
        // "every loaded template" — which must now also mean the null sentinel.
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        // The fan-out lands on the null group's targets and locks the host.
        await_locked(&null_host, "null-host").await;

        let msg = sess.job_cancel(&job_id).await.expect("cancel succeeds");
        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            saw_unlock(&null_host),
            "null-host's stranded lock was never removed: {msg}"
        );
        assert!(
            !still_locked(&null_host),
            "null-host is still locked after the force-abort release"
        );
    }

    /// The sentinel unlock is bounded by the same budget (#485): a null-group
    /// host whose lock read outruns it is reported with the *bare-tool* remedy
    /// (the sentinel has no RRID, so `-T <rrid>` does not apply), and the reply
    /// does not claim the lock is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_unlock_of_null_group_is_bounded_and_says_so_on_expiry() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        // Every SFTP read costs 400ms; the release below gets 200ms.
        let null_host = MockConnection::new("null-host")
            .with_sftp_session_delay(Duration::from_millis(400))
            .with_run_delay(Duration::from_secs(600));
        let target = Target::with_connection(
            "null-host",
            TargetState::Enabled,
            Box::new(null_host.clone()),
        );

        let sess = session(Config::default());
        {
            let mut guard = sess.session().lock().await;
            guard.targets_mut().add(target);
        }

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&null_host, "null-host").await;

        let budget = Duration::from_millis(200);
        let msg = sess
            .job_cancel_with_budget(&job_id, budget)
            .await
            .expect("cancel succeeds");

        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            msg.contains("lock state unknown on hosts attached with no report loaded"),
            "an expired sentinel release must be reported, with the bare-tool remedy: {msg}"
        );
        // The RRID-scoped remedy must not be shown for the RRID-less sentinel.
        assert!(
            !msg.contains("list_locks -T <rrid>"),
            "the sentinel has no RRID: {msg}"
        );
        assert!(!msg.contains("unlocked:"), "nothing was unlocked: {msg}");
        assert!(
            still_locked(&null_host),
            "null-host's lock is gone after all"
        );
    }

    /// A template that consumes the whole budget must not stop the sentinel
    /// release from being *attempted*: the sentinel holds a reserved share, so
    /// the blocked template is reported unknown while the null-group lock is
    /// still released.
    ///
    /// The block is the writer-preferring gate, not a slow host: a queued
    /// `load_template`-grade writer bumps `writers_waiting` for its whole wait,
    /// so `unlock_template`'s shared acquisition cannot proceed at all and no
    /// budget can outwait it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_template_cannot_starve_the_null_group_unlock() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        // One real timer await in the sentinel's release path, so the reserve
        // has to be a genuine slice of wall clock and not one lucky poll.
        let null_host =
            MockConnection::new("null-host").with_sftp_session_delay(Duration::from_millis(50));
        let mut null_target = Target::with_connection(
            "null-host",
            TargetState::Enabled,
            Box::new(null_host.clone()),
        );
        null_target.lock("").await.expect("null-host locked");

        let sess = session(Config::default());
        {
            let mut guard = sess.session().lock().await;
            guard.targets_mut().add(null_target);
        }
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        assert!(
            still_locked(&null_host),
            "fixture must arm the assertion — the sentinel holds a lock before the cancel"
        );

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));
        // `start_job` records no template scope, so the fallback resolves the
        // registry *and* the sentinel — the pass the budget has to cover.
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        let release = Arc::new(tokio::sync::Notify::new());
        let writer = tokio::spawn({
            let gate = sess.gate.clone();
            let release = Arc::clone(&release);
            async move {
                let _exclusive = gate.exclusive().await;
                release.notified().await;
            }
        });
        // Barrier: the writer's bump is visible once a shared acquisition can no
        // longer be taken. Without it the cancel could run before the writer
        // queued and the template would not block at all.
        let mut queued = false;
        for _ in 0..400 {
            if tokio::time::timeout(Duration::from_millis(5), sess.gate.shared())
                .await
                .is_err()
            {
                queued = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(queued, "the exclusive waiter never queued on the gate");

        let budget = Duration::from_millis(600);
        let before = Instant::now();
        let msg = sess
            .job_cancel_with_budget(&job_id, budget)
            .await
            .expect("cancel succeeds");
        let elapsed = before.elapsed();

        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            msg.contains(&format!("lock state unknown on {LOCK_RRID_A}")),
            "the blocked template must still be reported unknown: {msg}"
        );
        assert!(
            still_locked(&alpha) && !saw_unlock(&alpha),
            "the reply claimed a release the blocked template never performed: {msg}"
        );
        assert!(
            saw_unlock(&null_host),
            "the blocked template consumed the sentinel's share of the budget: {msg}"
        );
        assert!(
            !still_locked(&null_host),
            "the null group's lock survived the release: {msg}"
        );
        assert!(msg.contains("unlocked: null-host"), "got: {msg}");
        assert!(
            !msg.contains("hosts attached with no report loaded"),
            "the sentinel release did not time out, so its remedy must not appear: {msg}"
        );
        assert!(
            elapsed < CANCEL_GRACE + budget + Duration::from_millis(700),
            "reserving a share must not extend the pass beyond the budget: {elapsed:?}"
        );

        release.notify_one();
        writer.await.expect("gate writer panicked");
    }

    /// The other half of the split: reserving the sentinel a share must not
    /// starve the templates. Both groups hold a releasable lock and both are
    /// released inside one budget, with nothing reported unknown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_releases_the_template_and_the_null_group_in_one_budget() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        // A real timer await in each release path, so neither side can be
        // released on a single opportunistic poll of a zero-length slice.
        let alpha = MockConnection::new("host-alpha")
            .with_sftp_session_delay(Duration::from_millis(50))
            .with_run_delay(Duration::from_secs(600));
        let null_host =
            MockConnection::new("null-host").with_sftp_session_delay(Duration::from_millis(50));
        let mut null_target = Target::with_connection(
            "null-host",
            TargetState::Enabled,
            Box::new(null_host.clone()),
        );
        null_target.lock("").await.expect("null-host locked");

        let sess = session(Config::default());
        {
            let mut guard = sess.session().lock().await;
            guard.targets_mut().add(null_target);
        }
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess
            .job_cancel_with_budget(&job_id, Duration::from_millis(600))
            .await
            .expect("cancel succeeds");

        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            !still_locked(&alpha),
            "the sentinel's reserve starved the template's release: {msg}"
        );
        assert!(
            !still_locked(&null_host),
            "the null group's lock survived the release: {msg}"
        );
        assert!(
            msg.contains("unlocked: host-alpha, null-host"),
            "both groups must be reported released, templates first: {msg}"
        );
        assert!(!msg.contains("unknown"), "nothing timed out: {msg}");
    }

    /// A scoped force-cancel must **not** touch the sentinel (#485): an
    /// explicitly-RRID-scoped job resolves to that template only and could never
    /// have locked a null-group host, so the sentinel stays out of scope even
    /// when it holds a (deliberate) lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_force_cancel_leaves_the_null_group_lock_alone() {
        use mtui_hosts::Target;
        use mtui_types::enums::TargetState;

        let alpha = MockConnection::new("host-alpha");
        let null_host = MockConnection::new("null-host");
        // The sentinel host is locked *up front* (a deliberate hold) and must
        // survive the scoped job's cancel.
        let mut null_target = Target::with_connection(
            "null-host",
            TargetState::Enabled,
            Box::new(null_host.clone()),
        );
        null_target.lock("").await.expect("null-host locked");

        let sess = session(Config::default());
        {
            let mut guard = sess.session().lock().await;
            guard.targets_mut().add(null_target);
        }
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;

        // Scope to LOCK_RRID_A via start_jobs + `-T`-pinned argv: the recorded
        // scope is explicit, so `with_null` is false.
        let ids = sess
            .start_jobs(
                Arc::clone(&registry_with_probe(LockAndPark::new(Park::Forever))),
                "lock_and_park_probe",
                Vec::new(),
            )
            .await
            .expect("start_jobs succeeds");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        assert!(
            !saw_unlock(&null_host),
            "the scoped cancel touched the null group's lock: {msg}"
        );
        assert!(
            still_locked(&null_host),
            "the null group's lock must survive a scoped cancel"
        );
    }

    /// Hosts of the job's own group that it did not lock are untouched and
    /// unmentioned — the `run -t <subset>` case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_only_touches_the_hosts_the_job_locked() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        let beta = MockConnection::new("host-beta").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[("host-alpha", alpha.clone()), ("host-beta", beta.clone())],
        )
        .await;
        let registry = Arc::new(register_all());

        let ids = sess
            .start_jobs(
                Arc::clone(&registry),
                "run",
                vec!["-t".to_owned(), "host-alpha".to_owned(), "true".to_owned()],
            )
            .await
            .expect("start_jobs succeeds");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        // A whole-group release would report host-beta either way.
        assert!(
            !msg.contains("host-beta"),
            "a host the job never locked must not appear in the verdict: {msg}"
        );
        assert!(!saw_unlock(&beta), "host-beta was acted on");
    }

    /// An operator's `lock <comment>` reservation survives the cancel: a
    /// non-empty comment marks an **exclusive** hold (the PI assignment lock the
    /// session re-applies on every connect and reboot, or a manual reservation),
    /// while operation flows all stamp an empty comment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_leaves_a_comment_marked_reservation_alone() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        let reserved =
            MockConnection::new("host-reserved").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[
                ("host-alpha", alpha.clone()),
                ("host-reserved", reserved.clone()),
            ],
        )
        .await;
        let registry = Arc::new(register_all());

        // The real `lock` command: whole-group, carrying a comment.
        sess.run_command(
            &registry,
            "lock",
            &["-c".to_owned(), "reserved-for-me".to_owned()],
        )
        .await
        .expect("lock succeeds");
        assert!(still_locked(&reserved), "the reservation was not taken");
        assert!(still_locked(&alpha), "the reservation was not taken");

        // The job re-stamps host-alpha's lock with an empty (operation) comment
        // and hangs. Anchored on the *re-stamp*, since the exclusive create
        // already happened above.
        let ids = sess
            .start_jobs(
                Arc::clone(&registry),
                "run",
                vec!["-t".to_owned(), "host-alpha".to_owned(), "true".to_owned()],
            )
            .await
            .expect("start_jobs succeeds");
        await_relocked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        assert!(
            !msg.contains("host-reserved"),
            "a comment-marked reservation must not appear in the verdict: {msg}"
        );
        assert!(
            still_locked(&reserved),
            "the cancel removed an operator's reservation"
        );
    }

    /// A client-supplied `-T` is recorded as the job's scope, so the cancel never
    /// reaches for the other loaded template.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_honours_a_client_supplied_template_scope() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;
        let registry = Arc::new(register_all());

        let ids = sess
            .start_jobs(
                Arc::clone(&registry),
                "run",
                vec!["-T".to_owned(), LOCK_RRID_A.to_owned(), "true".to_owned()],
            )
            .await
            .expect("start_jobs succeeds");
        assert_eq!(ids.len(), 1, "-T narrows to one job: {ids:?}");
        await_locked(&alpha, "host-alpha").await;

        let msg = sess.job_cancel(&ids[0]).await.expect("cancel succeeds");
        assert!(msg.contains("unlocked: host-alpha"), "got: {msg}");
        assert!(
            !msg.contains(LOCK_RRID_B) && !msg.contains("host-beta"),
            "the unnamed template must not be in scope: {msg}"
        );
        assert!(!saw_unlock(&beta), "the other template was acted on");
    }

    /// The lingering active guard is dropped even when the release never reaches
    /// a single template — which is why it happens in a bounded preamble, before
    /// any gate or per-RRID wait. Here the per-template pass is blocked on the
    /// template's own dispatch lock for the whole budget, so a release that
    /// dropped the guard only *inside* that body would never drop it at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_clears_the_active_guard_even_when_the_release_cannot_run() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        // Unscoped fan-out over two templates: the exclusive, inline dispatch
        // path, which when aborted leaves its active guard on the canonical
        // session. It parks on A, so the guard is A's.
        let registry = registry_with_probe(LockAndPark::parking_on(Park::Forever, LOCK_RRID_A));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        // Block *both* templates' dispatch locks for the whole cancel. Taken
        // directly, not via `scoped_lock`, which would first queue on the gate
        // behind the job's exclusive hold and race the cancel.
        let blocker_a = sess.lock_for(LOCK_RRID_A).lock_owned().await;
        let blocker_b = sess.lock_for(LOCK_RRID_B).lock_owned().await;

        let budget = Duration::from_millis(200);
        let before = Instant::now();
        let msg = sess
            .job_cancel_with_budget(&job_id, budget)
            .await
            .expect("cancel succeeds");
        let elapsed = before.elapsed();

        assert!(
            msg.contains(&format!("lock state unknown on {LOCK_RRID_A}")),
            "the blocked template must be reported unknown: {msg}"
        );
        assert!(
            elapsed < CANCEL_GRACE + budget + Duration::from_millis(700),
            "the blocked release was not bounded: {elapsed:?}"
        );
        // Truthful: nothing was released.
        assert!(!saw_unlock(&alpha), "host-alpha was unlocked after all");

        drop(blocker_a);
        drop(blocker_b);
        let out = sess
            .run_command(
                &registry,
                "list_hosts",
                &["-T".to_owned(), LOCK_RRID_A.to_owned()],
            )
            .await
            .expect("list_hosts after a forced abort succeeds");
        assert!(
            out.contains("host-alpha"),
            "the active guard outlived the cancel, so the session reads the null \
             report: {out:?}"
        );
    }

    /// The budget covers the preamble too: a session mutex held by a live
    /// exclusive dispatch or a `get`/`put` transfer must not block `job_cancel`
    /// for that operation's whole duration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_does_not_block_on_a_busy_session() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        let registry = Arc::new(register_all());

        // The scoped path touches the canonical session only briefly (to fork),
        // so the test can hold the mutex while the job is parked mid host-op.
        let ids = sess
            .start_jobs(Arc::clone(&registry), "run", vec!["true".to_owned()])
            .await
            .expect("start_jobs succeeds");
        await_locked(&alpha, "host-alpha").await;
        let busy = sess.session().lock().await;

        let budget = Duration::from_millis(200);
        let before = Instant::now();
        // Bounded so the failure reads as "job_cancel blocked" rather than as a
        // hung test.
        let msg = tokio::time::timeout(
            Duration::from_secs(20),
            sess.job_cancel_with_budget(&ids[0], budget),
        )
        .await
        .expect("job_cancel must not block on the session mutex")
        .expect("cancel succeeds");
        let elapsed = before.elapsed();

        assert!(msg.contains("forced abort"), "got: {msg}");
        assert!(
            msg.contains("the session was busy, so the release never ran"),
            "a skipped release must say so: {msg}"
        );
        assert!(!msg.contains("unlocked:"), "nothing was unlocked: {msg}");
        assert!(
            elapsed < CANCEL_GRACE + budget + Duration::from_millis(700),
            "the preamble was outside the budget: {elapsed:?}"
        );
        assert!(still_locked(&alpha), "host-alpha's lock is gone after all");
        drop(busy);
    }

    /// A pass that releases one template and runs out of budget on the next
    /// reports both facts — the reply is not all-or-nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forced_cancel_reports_released_and_expired_templates_together() {
        let quick = MockConnection::new("host-quick");
        let slow =
            MockConnection::new("host-slow").with_sftp_session_delay(Duration::from_millis(400));

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-quick", quick.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-slow", slow.clone())]).await;

        // `start_job` records no scope, so the release falls back to both loaded
        // templates in registry order. The probe parks only on B, so A locks and
        // returns and *both* groups end up holding.
        let registry = registry_with_probe(LockAndPark::parking_on(Park::Forever, LOCK_RRID_B));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&quick, "host-quick").await;
        await_locked(&slow, "host-slow").await;

        let msg = sess
            .job_cancel_with_budget(&job_id, Duration::from_millis(200))
            .await
            .expect("cancel succeeds");
        assert!(
            msg.contains("unlocked: host-quick"),
            "the template that finished must be reported: {msg}"
        );
        assert!(
            msg.contains(&format!("lock state unknown on {LOCK_RRID_B}")),
            "the template that expired must be reported: {msg}"
        );
        assert!(still_locked(&slow), "host-slow's lock is gone after all");
    }

    /// A job scoped to an unloaded template has nothing of ours to release, and
    /// a release with nothing to report leaves the forced reply byte-identical.
    #[tokio::test]
    async fn forced_cancel_with_nothing_to_release_keeps_the_reply_unchanged() {
        let sess = session(Config::default());
        // A worker that never settles, recorded against an unloaded template.
        let job = Arc::new(StdMutex::new(Job {
            id: "probe-1".to_owned(),
            command: "probe".to_owned(),
            rrids: vec!["SUSE:Maintenance:9:9".to_owned()],
            state: JobState::Running,
            started: Instant::now(),
            finished: None,
            result: None,
            error: None,
            exit_code: None,
            handle: Some(tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(600)).await;
            })),
            cancel: CancellationToken::new(),
        }));
        sess.jobs.lock().unwrap().insert("probe-1".to_owned(), job);

        let msg = sess.job_cancel("probe-1").await.expect("cancel succeeds");
        assert_eq!(
            msg,
            format!(
                "cancelled job probe-1 (forced abort after {}s grace; a host operation \
                 already in flight may still finish on the host)",
                CANCEL_GRACE.as_secs()
            ),
            "a silent release must not perturb the pinned forced reply"
        );
    }

    // ---- client cancel of a synthesised command tool (PR #476) ------------- //

    /// The client's own cancel must not strand the remote operation lock any more
    /// than `job_cancel` does: mint a token, give the dispatch [`CANCEL_GRACE`],
    /// then force-abort and release on its behalf. Driven through the real `run`
    /// on two hosts blocked mid-op, so the abort lands between lock and unlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_cancel_frees_the_stranded_operation_lock() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        let beta = MockConnection::new("host-beta").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(
            &sess,
            LOCK_RRID_A,
            &[("host-alpha", alpha.clone()), ("host-beta", beta.clone())],
        )
        .await;
        let registry = Arc::new(register_all());

        let client_ct = CancellationToken::new();
        let call = tokio::spawn({
            let sess = Arc::clone(&sess);
            let registry = Arc::clone(&registry);
            let client_ct = client_ct.clone();
            async move {
                sess.run_command_client_cancellable(
                    &registry,
                    "run",
                    &["true".to_owned()],
                    None,
                    DEFAULT_PROGRESS_INTERVAL,
                    &client_ct,
                )
                .await
            }
        });

        await_locked(&alpha, "host-alpha").await;
        await_locked(&beta, "host-beta").await;

        let before = Instant::now();
        client_ct.cancel();
        let outcome = tokio::time::timeout(
            CANCEL_GRACE + ABORT_UNLOCK_BUDGET + Duration::from_secs(3),
            call,
        )
        .await
        .expect("client cancel must not hang")
        .expect("spawned task did not panic");
        assert!(
            before.elapsed() < CANCEL_GRACE + ABORT_UNLOCK_BUDGET + Duration::from_secs(2),
            "client cancel took too long: {:?}",
            before.elapsed()
        );

        let ToolOutcome::Aborted(unlock) = outcome else {
            panic!("a run blocked mid host-op must be force-aborted");
        };
        assert!(
            unlock
                .clause()
                .is_some_and(|c| c.contains("host-alpha") && c.contains("host-beta")),
            "the unlock verdict must name the hosts it released"
        );
        assert!(saw_unlock(&alpha), "host-alpha's lock was never removed");
        assert!(saw_unlock(&beta), "host-beta's lock was never removed");
        assert!(!still_locked(&alpha), "host-alpha is still locked");
        assert!(!still_locked(&beta), "host-beta is still locked");
    }

    /// A body observing the seam unwinds inside [`CANCEL_GRACE`], and its **own**
    /// verdict comes back as `Completed`, not a synthetic forced-abort error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_cancel_lets_a_cooperative_body_run_its_own_verdict() {
        let alpha = MockConnection::new("host-alpha");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        let registry = registry_with_probe(LockAndPark::new(Park::Seam));

        let client_ct = CancellationToken::new();
        let call = tokio::spawn({
            let sess = Arc::clone(&sess);
            let registry = Arc::clone(&registry);
            let client_ct = client_ct.clone();
            async move {
                sess.run_command_client_cancellable(
                    &registry,
                    "lock_and_park_probe",
                    &[],
                    None,
                    DEFAULT_PROGRESS_INTERVAL,
                    &client_ct,
                )
                .await
            }
        });

        await_locked(&alpha, "host-alpha").await;
        client_ct.cancel();

        let outcome = tokio::time::timeout(CANCEL_GRACE + Duration::from_secs(3), call)
            .await
            .expect("a cooperative body must unwind well inside the grace")
            .expect("spawned task did not panic");

        let ToolOutcome::Completed(result) = outcome else {
            panic!("a body that observes the seam must not be force-aborted");
        };
        let err = result.expect_err("the probe reports its own cancellation");
        assert_eq!(
            err.stderr, "cancelled",
            "the flow's own verdict must survive unchanged: {err:?}"
        );
        assert!(
            !err.stderr.contains("forced abort"),
            "a cooperative stop must not read as a forced abort: {err:?}"
        );
        assert!(
            !saw_unlock(&alpha),
            "the cooperative arm must not run the abort-path release"
        );
    }

    /// The exclusive dispatch path: a force-aborted unscoped fan-out leaves the
    /// canonical session holding the active entry's guard, so the dispatch future
    /// must be dropped *before* the unlock pass — otherwise the pass deadlocks on
    /// the entry (or, bounded, reports `stalled`) and a later scoped dispatch
    /// silently falls back to the null report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_cancel_on_the_exclusive_path_clears_the_active_guard() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        let registry = registry_with_probe(LockAndPark::new(Park::Forever));

        let client_ct = CancellationToken::new();
        let call = tokio::spawn({
            let sess = Arc::clone(&sess);
            let registry = Arc::clone(&registry);
            let client_ct = client_ct.clone();
            async move {
                sess.run_command_client_cancellable(
                    &registry,
                    "lock_and_park_probe",
                    &[],
                    None,
                    DEFAULT_PROGRESS_INTERVAL,
                    &client_ct,
                )
                .await
            }
        });

        await_locked(&alpha, "host-alpha").await;
        client_ct.cancel();

        // The unfixed deadlock/stall would hang here forever, not merely fail.
        let outcome = tokio::time::timeout(Duration::from_secs(20), call)
            .await
            .expect("run_command_client_cancellable must not deadlock on the lingering guard")
            .expect("spawned task did not panic");
        let ToolOutcome::Aborted(_) = outcome else {
            panic!("a body parked forever must be force-aborted");
        };
        assert!(saw_unlock(&alpha), "host-alpha's lock was never removed");
        assert!(!still_locked(&alpha), "host-alpha is still locked");

        // No active guard may remain; see
        // `forced_cancel_on_the_exclusive_path_unlocks_and_clears_the_active_guard`.
        let out = sess
            .run_command(
                &registry,
                "list_hosts",
                &["-T".to_owned(), LOCK_RRID_A.to_owned()],
            )
            .await
            .expect("list_hosts after a client cancel succeeds");
        assert!(
            out.contains("host-alpha"),
            "a lingering active guard sent the dispatch to the null report: {out:?}"
        );
    }

    // ---- progress heartbeats ----------------------------------------------- //

    #[derive(Default)]
    struct RecordingSink {
        calls: StdMutex<Vec<(f64, String)>>,
    }

    impl RecordingSink {
        fn calls(&self) -> Vec<(f64, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProgressSink for RecordingSink {
        fn report<'a>(
            &'a self,
            progress: f64,
            message: &'a str,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            let message = message.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push((progress, message));
            })
        }
    }

    /// Records the attempt then "fails". A `ProgressSink` swallows its own
    /// transport errors, so from the loop's view this is indistinguishable from a
    /// working sink — which is how the command result can be asserted to survive
    /// a send that would have failed.
    #[derive(Default)]
    struct FailingSink {
        calls: StdMutex<usize>,
    }

    impl ProgressSink for FailingSink {
        fn report<'a>(
            &'a self,
            _progress: f64,
            _message: &'a str,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                *self.calls.lock().unwrap() += 1;
                // Propagates nothing, mirroring the real rmcp sink, which logs a
                // send error at DEBUG and swallows it.
            })
        }
    }

    /// `sink = None` is zero-overhead: no frames, same stdout as `run_command`.
    #[tokio::test]
    async fn run_command_with_progress_none_emits_no_frames() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let registry = register_all();
        let sink = RecordingSink::default();

        let out = sess
            .run_command_with_progress(&registry, "whoami", &[], None, Duration::from_millis(1))
            .await
            .expect("whoami succeeds");
        assert!(out.starts_with("User: testuser"), "got: {out:?}");
        // The sink was built but never passed, so it recorded nothing.
        assert!(sink.calls().is_empty(), "no frames on the None path");
    }

    /// A slow future with a small interval fires >= 1 monotonic frame, each
    /// carrying the command name, and its output is returned unchanged. Driven
    /// over a controlled sleep to keep the timing deterministic.
    #[tokio::test]
    async fn run_with_heartbeat_fires_for_slow_future() {
        let sink = RecordingSink::default();
        let body = async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            "done"
        };

        let out =
            run_with_heartbeat(body, &sink, "_sleepy_command", Duration::from_millis(50)).await;
        assert_eq!(out, "done", "future output returned unchanged");

        let calls = sink.calls();
        assert!(!calls.is_empty(), "at least one heartbeat fired: {calls:?}");
        for (progress, message) in &calls {
            assert!(*progress >= 0.0, "progress non-negative");
            assert!(
                message.contains("_sleepy_command"),
                "frame names the command: {message:?}"
            );
        }
        let values: Vec<f64> = calls.iter().map(|(p, _)| *p).collect();
        let mut sorted = values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(values, sorted, "progress monotonic: {values:?}");
    }

    /// A future finishing well inside the interval fires zero frames.
    #[tokio::test]
    async fn run_with_heartbeat_no_frames_for_fast_future() {
        let sink = RecordingSink::default();
        let out = run_with_heartbeat(async { 7 }, &sink, "fast", Duration::from_secs(1)).await;
        assert_eq!(out, 7);
        assert!(sink.calls().is_empty(), "no frames: {:?}", sink.calls());
    }

    /// The heartbeat path passes `McpCommandError` through unchanged.
    #[tokio::test]
    async fn run_command_with_progress_propagates_command_error() {
        let sess = session(Config::default());
        let registry = register_all();
        let sink = RecordingSink::default();

        let err = sess
            .run_command_with_progress(
                &registry,
                "no_such_command",
                &[],
                Some(&sink),
                Duration::from_millis(50),
            )
            .await
            .expect_err("unknown command must fail");
        assert_eq!(err.exit_code, 1, "unknown command is exit 1");
    }

    /// A sink whose send would fail must not mask the command result. Driven on a
    /// paused clock so the schedule is exact rather than a race against wall
    /// time: tokio auto-advances to each next timer, so the ticks land at virtual
    /// 40/80/120ms and the body completes at 150ms — three attempted sends, every
    /// run, where the wall-clock version could see zero on a starved CPU.
    #[tokio::test(start_paused = true)]
    async fn run_with_heartbeat_send_failure_does_not_mask_result() {
        let sink = FailingSink::default();
        let body = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            "ok"
        };

        let out =
            run_with_heartbeat(body, &sink, "_sleepy_command", Duration::from_millis(40)).await;
        assert_eq!(out, "ok", "result survives a failing sink");
        assert_eq!(
            *sink.calls.lock().unwrap(),
            3,
            "every tick before completion attempted a send (40/80/120ms)"
        );
    }
}
