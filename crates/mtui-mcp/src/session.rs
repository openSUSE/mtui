//! Per-client MCP session.
//!
//! [`McpSession`] is the headless mtui session that backs one `mtui-mcp` client.
//! It owns the mutable [`Session`] state a command dispatches against plus the
//! [`SharedBuf`] sink that captures the command's display output for the tool
//! result, and
//! exposes [`run_command`](McpSession::run_command) — the central dispatch
//! primitive the tool layer calls (drain → dispatch → capture → output-cap).
//! `run_command` also supplies the [`McpCommandError`] failure envelope and
//! the per-result output cap (`[mcp] max_output_bytes`); the non-interactive
//! contract (`interactive = false`, unset prompter) is provided by
//! `capture::session` passing `is_repl = false`.
//!
//! Under **stdio** one instance serves the single client; under **http** the
//! `SessionRegistry` owns one instance per client. In both cases
//! the [`crate::provider::SessionProvider`] seam hands callers an
//! `Arc<McpSession>`, so the tool layer stays transport-agnostic.
//!
//! ## Four mechanisms
//!
//! **Per-template lock discipline.** A shared/exclusive registry gate
//! ([`crate::concurrency::RwGate`]) plus a lazily-created per-RRID lock map.
//! `command_lock` takes the gate *shared* + one per-RRID lock for a
//! single-template call (so same-RRID calls serialise and different-RRID
//! calls take distinct locks) and the gate *exclusive* for fan-out / registry
//! mutators; [`scoped_lock`](McpSession::scoped_lock) is the same hold for
//! the hand-written testreport tools. This also gives genuine wall-clock
//! concurrency between *different-RRID* calls plus per-call output isolation:
//! a single-real-RRID call dispatches on a [`Session::fork_for_call`] (which
//! shares the loaded reports' per-entry `Arc<Mutex<..>>` locks and carries its
//! own display) via [`dispatch_command`], spawned so it overlaps a concurrent
//! different-RRID call; [`run_command`](McpSession::run_command) does not hold
//! a session-wide mutex across the scoped dispatch. Registry-structure
//! mutators ([`Command::mutates_registry`](mtui_core::Command::mutates_registry))
//! and unscoped fan-out still take the gate *exclusive* against the canonical
//! session.
//!
//! **Background-job table** (`_jobs`). A slow
//! `run`/`update`/`downgrade` can be started with
//! [`start_jobs`](McpSession::start_jobs) (one job per resolved template, each
//! `-T <rrid>`-scoped) and returns a handle immediately instead of holding the
//! request open; the outcome is polled via
//! [`job_status`](McpSession::job_status) / [`job_result`](McpSession::job_result)
//! and controlled via [`job_list`](McpSession::job_list) /
//! [`job_cancel`](McpSession::job_cancel). Each job worker runs through the same
//! [`run_command`](McpSession::run_command) primitive (so it takes the same
//! per-RRID / registry gate and output cap as a foreground call). The table's
//! resource use is bounded: a spawn is rejected
//! (before allocating a worker) once the session holds
//! `[mcp] max_active_jobs` running jobs — a fan-out is admitted or rejected as a
//! whole — and terminal records are FIFO-evicted to `[mcp] max_completed_jobs`
//! so a long-lived session does not accumulate job history unbounded (`0`
//! disables either cap). The capture sink is likewise bounded at *write time*
//! (see [`crate::capture`]) so a single command cannot buffer more than
//! `[mcp] max_output_bytes` before the cap applies.
//!
//! **Progress heartbeats** (`notifications/progress`). A long-running
//! foreground tool call (`run_command_with_progress`) races the
//! dispatch against a ticker that emits a progress frame every
//! `DEFAULT_PROGRESS_INTERVAL` against a transport-free [`ProgressSink`], so an
//! MCP client that honours the protocol's progress contract does not time out on
//! `run`/`update`/`set_repo`/`commit`. The rmcp-backed sink (peer +
//! `progressToken`) is built in [`crate::server`] from the request context; this
//! layer stays rmcp-free. A `None` sink takes the original zero-overhead path.
//!
//! **[`close`](McpSession::close).** The session
//! teardown the http `SessionRegistry` calls on
//! eviction. For **every** loaded template it releases the report's pool claims
//! then disconnects its host group, best-effort + idempotent, under a bounded
//! [`HOST_CLOSE_TIMEOUT`] so a wedged host close cannot block the idle-sweep.
//! It does not empty each `HostsGroup` (a closed `Target` is left in the group
//! with a dead connection, dropped whole with the report) and it bounds the
//! wait with [`tokio::time::timeout`].
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

/// Default interval between `notifications/progress` heartbeat frames while a
/// long-running foreground tool call runs.
///
/// Not a config key: it is the default the tool layer passes
/// to [`McpSession::run_command_with_progress`], overridable per call so tests
/// can drive a sub-second interval.
pub(crate) const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Cooperative grace [`job_cancel`](McpSession::job_cancel) gives a worker
/// between cancelling its token and force-aborting its task.
///
/// A dispatch parked at a seam checkpoint (the `Command::run` driver's
/// pre-flight / between-templates checks, or a body watching
/// [`mtui_core::Session::cancel_requested`]) settles well inside this; a body
/// blocked mid host-op never observes the token and burns the full grace
/// before the hard abort. Kept short so `job_cancel` stays responsive; not a
/// config key.
pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// Wall-clock budget for the whole post-abort operation-lock release
/// ([`McpSession::unlock_after_abort`]), across every template the cancelled
/// job was scoped to.
///
/// A force-aborted dispatch never reached its own `unlock()`, so the cancel
/// releases the operation lock on its behalf — but the release is an SSH
/// round-trip per host and may queue behind another template's in-flight
/// dispatch, so it is bounded to keep `job_cancel` responsive (the same reason
/// [`CANCEL_GRACE`] is short). On expiry the remaining locks stay held and the
/// reply says so; the `unlock` command and the fleet's stale-lock reap remain
/// the backstop. Not a config key.
pub(crate) const ABORT_UNLOCK_BUDGET: Duration = Duration::from_secs(5);

/// A [`JoinHandle`] wrapper that aborts its task when dropped.
///
/// The concurrent dispatch path ([`McpSession::run_command`]) runs the command
/// body on a spawned task and awaits it. If the awaiting `run_command` future is
/// itself cancelled (an aborted background-job worker, or a dropped request
/// future), this guard aborts the spawned dispatch too — preserving the inline
/// path's cancellation shape (the body's future is dropped, not detached to run
/// on unobserved).
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A transport-free sink for heartbeat progress frames.
///
/// Models the single progress-reporting call the heartbeat loop consumes.
/// Keeping it a trait (rather than importing the rmcp `Peer`) keeps this
/// crate's session layer transport-free and unit-testable
/// with a recording double; the rmcp-backed implementation
/// (`crate::server::PeerProgressSink`) is built from the request context and sends
/// a real `notifications/progress`.
///
/// Implementors **must not** propagate transport failures: a send error is the
/// concern of the sink (log at DEBUG and swallow) so a flaky client can never mask
/// the command's actual outcome.
///
/// [`report`](ProgressSink::report) returns a boxed future (rather than a native
/// `async fn`) to keep the trait `dyn`-compatible without pulling `async-trait`
/// into this always-compiled library layer; the heartbeat loop only ever holds a
/// `&dyn ProgressSink`.
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
/// `fut` is already async, so this drives it via [`tokio::select!`] against a
/// ticker. Each tick reports the elapsed seconds and a `"<command> running
/// (<n>s)…"` message. Progress values are monotonic (elapsed since start).
/// When `fut` completes first its output is returned unchanged; a heartbeat is
/// never emitted after completion.
///
/// The sink swallows its own transport errors (see [`ProgressSink`]), so this loop
/// cannot mask `fut`'s result.
pub(crate) async fn run_with_heartbeat<F>(
    fut: F,
    sink: &dyn ProgressSink,
    command: &str,
    interval: Duration,
) -> F::Output
where
    F: Future,
{
    // `progress` is measured on the std clock, which tokio's `start_paused`
    // does not advance — a test on virtual time sees 0.0 on every frame, and an
    // assertion about progress values would hold vacuously there. Tick *counts*
    // stay exact under a paused clock (the ticker is a tokio timer), so pin the
    // schedule with counts on virtual time and the values on the wall clock.
    let started = Instant::now();
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Bias the future so a body that finishes exactly on a tick boundary
            // returns rather than emitting a spurious final frame.
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
/// It carries the streams captured during the failed run so the server layer
/// can surface them to the client:
///
/// * `stdout` — everything the command printed before failing (already capped).
/// * `stderr` — the parse/usage complaint or the command-error message.
/// * `exit_code` — argparse-style status: `2` for a usage/parse error, `1` for
///   an unknown command or a command-body failure.
///
/// [`Display`](std::fmt::Display) renders a one-line summary plus the captured
/// stderr so the default MCP error envelope is human-readable.
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
/// and the poll methods (which read it), so it lives behind an
/// `Arc<StdMutex<Job>>` in [`McpSession::jobs`]. The `StdMutex` is only ever
/// held for a field read/write, never across an `.await`.
#[derive(Debug)]
struct Job {
    /// The session-unique job id (`"<command>-<n>"` or `"<command>-<rrid>-<n>"`).
    id: String,
    /// The command name.
    command: String,
    /// The templates this job's dispatch resolves to, recorded at mint time.
    ///
    /// Scopes the post-abort operation-lock release
    /// ([`McpSession::unlock_after_abort`]) to the templates this job could
    /// have locked hosts on, so cancelling one job never disturbs a concurrent
    /// job's locks on another template. Recorded at mint (from the resolution
    /// [`McpSession::start_jobs`] already performed) rather than re-derived at
    /// cancel time: the loaded set may have changed since, and the job id
    /// carries the RRID only as a `:`-mangled string.
    ///
    /// **Empty** means the scope is unknown — the single-job
    /// [`start_job`](McpSession::start_job) primitive is synchronous and cannot
    /// resolve, and an argv that resolves to nothing real has no scope to
    /// record. The cancel path then falls back to every loaded template, which
    /// is the conservative answer for a dispatch that may have taken the
    /// registry gate exclusively and locked hosts across all of them.
    rrids: Vec<String>,
    /// The current lifecycle state.
    state: JobState,
    /// When the job was minted (for `elapsed_s`).
    started: Instant,
    /// When the job reached a terminal state (frozen `elapsed_s` afterwards).
    finished: Option<Instant>,
    /// The captured stdout on success, or the pre-failure stdout on failure.
    result: Option<String>,
    /// The failure summary (`McpCommandError` stderr) when `state == Failed`.
    error: Option<String>,
    /// The failure exit code when `state == Failed`.
    exit_code: Option<i32>,
    /// The worker task handle, aborted by [`McpSession::job_cancel`] when the
    /// cooperative grace elapses.
    handle: Option<JoinHandle<()>>,
    /// This job's cancellation token, installed on the session its dispatch
    /// runs on. [`McpSession::job_cancel`] cancels it *first* so a body
    /// observing the seam ([`mtui_core::Session::cancel_requested`]) can stop
    /// cooperatively before the hard abort.
    cancel: CancellationToken,
}

/// A public, poll-facing snapshot of a `Job` (no task handle).
///
/// The job tools render it into the one-line status text.
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
/// ([`McpSession::unlock_after_abort`]) actually achieved.
///
/// Kept as disjoint buckets rather than a formatted string so the reply can
/// distinguish "released" from "left alone" from "failed" from "never got
/// there" — a forced cancel must never claim a release it did not perform.
#[derive(Debug, Default)]
struct AbortUnlock {
    /// Hosts whose hold this pass dropped. Because the fan-out is scoped to
    /// locks the job's own group actually held
    /// ([`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held)), a
    /// host that was never locked is not in the map at all and never lands
    /// here.
    unlocked: Vec<String>,
    /// Hosts whose lock belongs to another owner — left untouched (benign).
    contended: Vec<String>,
    /// Hosts whose release hit a real transport error, with the reason. The
    /// lock is still held there.
    failed: Vec<(String, String)>,
    /// Templates the budget expired on, whether before their entry was reached
    /// or part-way through their host fan-out. In the second case some hosts
    /// may in fact have been released, but the fan-out future was dropped and
    /// its partial outcome map went with it — this pass reports the whole
    /// template as unknown rather than guessing which half succeeded.
    unknown: Vec<String>,
    /// The budget expired in the preamble, before the scope was even resolved:
    /// the pass never ran and no template can be named.
    stalled: bool,
}

impl AbortUnlock {
    /// Folds one template's
    /// [`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held)
    /// outcome map into the buckets.
    fn absorb(&mut self, outcomes: BTreeMap<String, LockOutcome>) {
        for (host, outcome) in outcomes {
            match outcome {
                LockOutcome::Released => self.unlocked.push(host),
                LockOutcome::Contended => self.contended.push(host),
                LockOutcome::Failed(reason) => self.failed.push((host, reason)),
                // Unreachable on an unlock fan-out. Ignored rather than folded
                // into a bucket, matching how the `unlock` command's own match
                // treats it — inventing a verdict for an impossible outcome is
                // how a reply starts claiming things that did not happen.
                LockOutcome::Acquired => {}
            }
        }
    }

    /// The clause appended to the forced-cancel reply, or `None` when there was
    /// nothing to say (no template in scope, or no held lock in any of them).
    ///
    /// Silence is deliberate: a cancel that had no host lock to act on must
    /// leave the reply byte-identical to what it has always been.
    ///
    /// The remedies are deliberately *scoped*. The usual cause of an expired
    /// release is that a **successor** dispatch already holds the template —
    /// telling a client to run a bare `unlock` there would have it strip a live
    /// operation's lock, which is the very failure this whole change exists to
    /// stop.
    fn clause(&self) -> Option<String> {
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

/// Pins `argv` to `rrid` by **prepending** `-T <rrid>`.
///
/// Prepended, not appended: a positional `REMAINDER` command like `run` would
/// swallow a trailing `-T <rrid>` into its own value.
///
/// Left untouched when `argv` already carries an explicit scope flag:
/// * `-T`/`--template` — already pinned, and a second one is redundant;
/// * `--all-templates` — declared `conflicts_with("template")`, so adding `-T`
///   would turn the dispatch into a *parse error* instead of scoping it. The
///   caller asked for every template, so the job stays unpinned (and its
///   recorded scope can drift if the loaded set changes mid-flight — the
///   pre-existing behaviour for that flag).
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

/// Process-global monotonic source of [`McpSession::id`] values. Each session
/// pulls a fresh id at construction, so two distinct sessions never share one
/// (freshness independent of heap-address reuse).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// A headless mtui session backing one MCP client.
///
/// Holds the [`Session`] behind a [`Mutex`] because command dispatch
/// ([`mtui_core::dispatch_argv`]) needs `&mut Session` while the rmcp
/// `ServerHandler` methods take `&self`. The paired
/// [`SharedBuf`] is the sink the session's display writes to; a tool call
/// [`take`](SharedBuf::take)s it to isolate its own output.
pub struct McpSession {
    /// Process-unique, monotonic id assigned at construction. Stable for the
    /// session's lifetime; two distinct sessions never share one. Used to assert
    /// session freshness without relying on `Arc` address identity.
    id: u64,
    /// The guarded session commands dispatch against.
    session: Arc<Mutex<Session>>,
    /// The capture sink the session's display writes to; drained per tool call.
    output: SharedBuf,
    /// Per-result output-size budget (bytes), from `config.mcp_max_output_bytes`.
    /// `0` disables the cap. Retained here so [`run_command`](Self::run_command)
    /// need not hold the whole [`Config`].
    max_output_bytes: usize,
    /// Source read-size budget (bytes), from `config.mcp_max_input_bytes`. `0`
    /// disables the cap. Bounds how much of an on-disk checkout file the
    /// hand-written `testreport_read` tool reads before stopping.
    max_input_bytes: usize,
    /// Tool-surface profile (`config.mcp_profile`), consumed by
    /// [`McpServer::new`](crate::server::McpServer::new) to narrow the exposed
    /// tools. Retained here (with the two override lists below) for the same
    /// reason as `max_output_bytes`: the server holds the session, not the config.
    profile: String,
    /// Extra tools to keep on top of the profile (`config.mcp_tools_allow`).
    tools_allow: Vec<String>,
    /// Tools to remove regardless of profile/allow (`config.mcp_tools_deny`).
    tools_deny: Vec<String>,
    /// The registry shared/exclusive gate.
    ///
    /// A command scoped to exactly one template enters this in *shared* mode
    /// (so it cannot overlap a registry mutation); registry mutators
    /// (`load_template`/`unload`) and unscoped fan-out enter it *exclusive*,
    /// draining in-flight per-RRID work. See [`command_lock`](Self::command_lock).
    gate: RwGate,
    /// Lazily-created per-RRID locks.
    ///
    /// Same-RRID calls share one `Arc<Mutex<()>>` and serialise; different-RRID
    /// calls take different locks. The outer [`StdMutex`] guards the map's own
    /// lazy population (held only for the get-or-insert, never across an await).
    rrid_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The background-job table, keyed by job id.
    ///
    /// A backgrounded slow command runs in a spawned worker that records its
    /// outcome on its `Arc<StdMutex<Job>>`; the poll methods
    /// ([`job_status`](Self::job_status) / [`job_result`](Self::job_result))
    /// read it without locking the session. Under http the registry's idle
    /// sweep drops the whole session and its table with it; within a session's
    /// lifetime the table is **bounded** — active spawns are capped by
    /// [`max_active_jobs`](Self::max_active_jobs) and terminal records are
    /// FIFO-evicted to [`max_completed_jobs`](Self::max_completed_jobs). The
    /// outer [`StdMutex`] guards insert/lookup/eviction
    /// only (never held across an await).
    jobs: StdMutex<HashMap<String, Arc<StdMutex<Job>>>>,
    /// Monotonic job-id counter, pre-incremented per minted job so ids are
    /// session-unique.
    job_counter: AtomicU64,
    /// Ceiling on concurrent *running* jobs (`config.mcp_max_active_jobs`); a
    /// spawn request that would exceed it is rejected before allocating the
    /// worker. `0` disables the cap.
    max_active_jobs: usize,
    /// Ceiling on retained *terminal* job records (`config.mcp_max_completed_jobs`);
    /// the oldest-finished records beyond it are evicted FIFO. `0` disables the
    /// cap.
    max_completed_jobs: usize,
}

/// An acquired hold on the concurrency gate for one command/tool invocation.
///
/// Returned by `McpSession::command_lock` / [`McpSession::scoped_lock`] and
/// kept alive for the duration of the critical section; dropping it releases the
/// gate (and any per-RRID lock) in the right order. The fields are never read —
/// they exist to own the guards — hence the leading underscores.
#[must_use = "dropping the CommandLock immediately releases the gate"]
pub enum CommandLock {
    /// A single-template hold: the registry gate shared **plus** one per-RRID
    /// lock. The `_rrid` guard drops first (declaration order), then `_shared`,
    /// matching the acquire order (gate-shared → rrid lock) in reverse.
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
    /// capture sink, and returns it as an `Arc` (the shape the provider hands
    /// out).
    ///
    /// The session is non-interactive with color disabled — see
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

    /// The process-unique, monotonic id assigned at construction.
    ///
    /// Stable for the session's lifetime; two distinct sessions never share one.
    /// A valid freshness signal where `Arc` address identity is not (a freed
    /// address can be reused by the allocator).
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
    /// Exposed for the hand-written testreport tools ([`crate::testreport_tools`]),
    /// which cap their file-content payloads with the same
    /// [`cap_output`] budget `run_command` applies.
    #[must_use]
    pub(crate) fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// The configured source read-size budget (bytes); `0` disables it.
    ///
    /// Exposed for the hand-written [`testreport_read`](crate::testreport_tools)
    /// tool, which stops reading a checkout file once this many bytes have been
    /// consumed (appending a truncation notice) so a huge or slow file cannot
    /// exhaust memory.
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

    /// Returns (creating on first use) the per-template lock for `rrid`.
    ///
    /// Lazily populates [`rrid_locks`](Self::rrid_locks) under its guard so two
    /// tasks racing to lock the same fresh RRID share one lock object.
    fn lock_for(&self, rrid: &str) -> Arc<Mutex<()>> {
        let mut map = self.rrid_locks.lock().expect("rrid lock map poisoned");
        Arc::clone(
            map.entry(rrid.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Acquires the right lock(s) for a `name`/`argv` invocation and returns a
    /// guard holding them for the caller's critical section.
    ///
    /// Resolves exactly as the foreground dispatch does (via
    /// [`resolve_command_rrids`]):
    ///
    /// * resolves to **exactly one** loaded template → the registry gate in
    ///   *shared* mode **plus** that template's per-RRID lock, so different-RRID
    ///   commands run concurrently while same-RRID commands serialise and no
    ///   command overlaps a registry mutation;
    /// * fan-out / unscoped-multi commands, registry mutators
    ///   (`load_template`/`unload`), or anything that resolves to no real
    ///   template → the registry gate in *exclusive* mode, which drains in-flight
    ///   per-RRID commands and blocks new ones for the duration.
    ///
    /// A single call never holds two per-RRID locks and the exclusive path holds
    /// only the gate, so the lock order (gate-shared → one rrid lock) is total
    /// and cannot deadlock. Resolution needs the [`Session`] (loaded set + active
    /// pointer), so it briefly locks the session — released before the returned
    /// guard is handed back, so the caller may re-lock the session for dispatch.
    async fn command_lock(&self, registry: &Registry, name: &str, argv: &[String]) -> CommandLock {
        let rrids = match registry.get(name) {
            // A registry-structure mutator (`load_template`/`unload`/`switch`/
            // `regenerate`) must take the gate *exclusive* even when it resolves
            // to a single template: the concurrent path dispatches on a per-call
            // fork whose registry snapshot is discarded, so a structural mutation
            // would be lost unless it runs against the canonical session under
            // the exclusive gate.
            Some(command) if command.mutates_registry() => None,
            Some(command) => {
                let session = self.session.lock().await;
                resolve_command_rrids(command.as_ref(), &session, argv)
            }
            // Unknown command: no meaningful scope, serialise conservatively.
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

    /// The registry gate in exclusive mode — the hold the hand-written
    /// transfer tools (`get`/`put`, #434) take around their host fan-outs,
    /// matching how synthesized fan-out commands serialise today
    /// ([`command_lock`](Self::command_lock)'s `_ =>` arm): `Session::activate`
    /// may only be flipped under the exclusive gate.
    pub(crate) async fn exclusive_lock(&self) -> CommandLock {
        CommandLock::Exclusive(self.gate.exclusive().await)
    }

    /// Holds the registry-shared gate plus one template's per-RRID lock.
    ///
    /// For the hand-written testreport tools (which act on a single template's
    /// files): entering the gate *shared* keeps the loaded set stable for the
    /// body (no concurrent `load_template`/`unload`) while still letting tools on
    /// *other* templates run in parallel, and the per-RRID lock serialises
    /// against foreground dispatch for the *same* template (e.g. a concurrent
    /// `commit`).
    ///
    /// `rrid` is the resolved target template id, or `None` to fall back to the
    /// active template (single-/zero-loaded case). Callers should resolve and
    /// validate the target report *inside* the body, where the shared gate
    /// guarantees the registry cannot change underfoot.
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
    /// Owned by the http `SessionRegistry`, which calls
    /// it when it evicts a session (idle-TTL sweep or explicit eviction).
    /// Mirrors the REPL `quit`
    /// disconnect path — `HostsGroup::close` per
    /// template, its per-host `Target::close` fanning out concurrently — but
    /// **without** the exit-flag / history-flush tail, since the process keeps
    /// serving other clients.
    ///
    /// **Every** loaded template's hosts are disconnected, not just the active
    /// one's: a session may hold several templates at once (each owning its own
    /// host group), and evicting the session must reap all of them — matching the
    /// REPL `quit` command.
    ///
    /// The whole teardown is best-effort and idempotent: for each template it
    /// releases the report's host-arbitration pool claims (in-process ownership +
    /// remote pool locks; a no-op when pool selection was never used) then closes
    /// its host group. A second call re-runs both over already-released claims and
    /// already-closed targets, both no-ops. The fan-out is bounded by
    /// [`HOST_CLOSE_TIMEOUT`]: a wedged host close is logged and abandoned so
    /// `close()` — and the registry idle-sweep awaiting it — always returns.
    ///
    /// `HostsGroup::close` (like the REPL `quit`) closes each `Target` but leaves
    /// it in the group with its now-dead connection — the report and its host
    /// group are dropped whole when the session is evicted. So this does not
    /// empty the groups; a closed target simply reports its connection
    /// inactive/closed.
    pub async fn close(&self) {
        self.close_with_timeout(HOST_CLOSE_TIMEOUT).await;
    }

    /// [`close`](Self::close) with an explicit fan-out budget.
    ///
    /// The timeout seam exists so the (colocated) wedged-close unit test can
    /// bound the wait to a fraction of a second instead of the full
    /// [`HOST_CLOSE_TIMEOUT`].
    async fn close_with_timeout(&self, timeout: Duration) {
        // Snapshot every loaded entry's lockable handle under the session lock,
        // then drop the session guard *before* the teardown awaits: holding the
        // `MutexGuard<Session>` across the per-entry `.await` would force the
        // whole close future to require `Session: Sync` (which it is not — the
        // display sink is `Send`-only). The `Arc<Mutex<..>>` handles keep each
        // report alive independently, so teardown needs no `&Session`.
        let handles: Vec<_> = {
            let mut session = self.session.lock().await;
            // Release any lingering active handle before locking entries: a prior
            // dispatch leaves the active template's entry locked via the session's
            // per-call guard, and this loop locks *every* entry to tear it down —
            // which would self-deadlock on the active one otherwise.
            session.release_active_guard();
            session
                .templates
                .rrids()
                .into_iter()
                .filter_map(|rrid| session.templates.handle(&rrid))
                .collect()
        };
        let teardown = async {
            for entry in handles {
                let mut report = entry.lock().await;
                // Release arbiter ownership + remote pool locks before
                // disconnecting (best-effort; a no-op without pooling).
                report.release_pool_claims().await;
                // Close the group: plain disconnect (no reboot/poweroff on an
                // MCP session eviction, unlike the REPL `quit` bootarg).
                // Per-host teardown outcomes are irrelevant on eviction.
                let _ = report.base_mut().targets.close(None).await;
            }
        };
        // Never let a wedged host teardown block the eviction (and the http
        // idle-sweep behind it): abandon the fan-out past the budget.
        if tokio::time::timeout(timeout, teardown).await.is_err() {
            tracing::warn!("host disconnect timed out after {timeout:?}; abandoning teardown");
        }
    }

    /// Runs a registered command and returns its captured, output-capped stdout.
    ///
    /// The central MCP dispatch primitive: it dispatches `name`/`argv` through
    /// the **same** engine the REPL uses (a forked-session [`dispatch_command`] on the
    /// concurrent path, [`dispatch_argv`] on the canonical session for the
    /// exclusive path), then returns what the command wrote to the call's own
    /// captured display — passed through `cap_output` so one large result cannot
    /// dwarf the client's context.
    ///
    /// Before dispatch the call takes its `command_lock`:
    /// a single-template call holds the registry gate *shared* plus its per-RRID
    /// lock (so same-RRID calls serialise, different-RRID calls take distinct
    /// locks), while fan-out / mutators take the gate *exclusive*. A single-RRID
    /// (non-mutator) call then dispatches on a
    /// [`Session::fork_for_call`](mtui_core::Session::fork_for_call) — sharing the
    /// report entry locks, with its own display — spawned so it runs in genuine
    /// parallel with a concurrent different-RRID call; the
    /// exclusive path dispatches on the canonical session so its config/registry
    /// mutations persist. A `--help`/`--version` request is a *success* (its text
    /// is returned), matching argparse's exit-0 semantics.
    ///
    /// # Errors
    ///
    /// Returns [`McpCommandError`] when argument parsing fails
    /// (`exit_code == 2`), the command is unknown, or the command body fails
    /// (`exit_code == 1`). The error carries the (capped) stdout produced before
    /// the failure plus the failure text as stderr.
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
    /// foreground tool calls pass `None` and keep the session's own (never
    /// cancelled) token. On the forked (scoped) path the token is set on the
    /// per-call fork; on the exclusive path it is swapped onto the canonical
    /// session for the duration of the dispatch and restored after.
    async fn run_command_cancellable(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        cancel: Option<CancellationToken>,
    ) -> Result<String, McpCommandError> {
        // Acquire the per-template / registry-gate hold for this invocation
        // *before* touching the session, so same-RRID and unscoped calls
        // serialise and mutators drain in-flight per-RRID work. Held for the
        // whole dispatch, released when `_lock` drops at end of scope.
        let lock = self.command_lock(registry, name, argv).await;

        // Per-call output isolation: give this
        // dispatch its *own* fresh capture buffer + display so two overlapping
        // calls never write into the same buffer and clobber each other's stdout.
        // Bounded to the same budget as the session-wide sink.
        let call_buf = SharedBuf::with_limit(self.max_output_bytes);
        let call_display =
            CommandPromptDisplay::with_sink(Box::new(call_buf.clone()), ColorMode::Never);

        let result = match &lock {
            // Concurrent path: a single-real-RRID
            // call holds the gate *shared* + its per-RRID lock. Fork a per-call
            // `Session` that *shares* the loaded reports' per-entry locks (so this
            // call locks only its own template's entry) and dispatch on it —
            // **without** holding the canonical session mutex across dispatch, so a
            // concurrent different-RRID call runs in genuine parallel. The forked
            // session is snapshotted under the (briefly held) canonical lock, which
            // the shared gate keeps consistent (no `Scope::Single` mutator can run
            // concurrently). Report *content* mutations are visible to the canonical
            // session (same `Arc<Mutex<..>>`); the fork's own config/registry
            // structure is discarded, which is sound because a per-RRID command
            // never mutates them (that is the exclusive path below).
            CommandLock::Scoped { .. } => {
                // Snapshot the forked session under the (briefly held) canonical
                // lock, then dispatch the command on a *spawned* task so a
                // blocking body overlaps a concurrent different-RRID call in real
                // wall-clock time (the caller drives us via `join!`/`join_all` on
                // one task; only a separate task yields genuine parallelism). The
                // command is resolved to an owned `Arc<dyn Command>` and argv is
                // cloned, so the spawned future borrows neither the registry nor
                // the caller's argv — it is `Send + 'static`. `command_lock`
                // already proved this resolves to exactly one loaded template, so
                // the command is registered.
                let command = registry
                    .get(name)
                    .expect("scoped lock implies a resolvable command")
                    .clone();
                let mut call_session = {
                    let session = self.session.lock().await;
                    session.fork_for_call(call_display)
                };
                // Wire the per-job token into the fork so the dispatched body
                // (and the `Command::run` driver's checkpoints) observe a
                // `job_cancel` cooperatively. Foreground calls get a fresh
                // (never-cancelled) token rather than the fork's inherited one:
                // a hard-aborted exclusive dispatch can leave a cancelled job
                // token behind on the canonical session (its restore is skipped
                // when the worker future is dropped), and the fork must not
                // inherit that staleness — installing unconditionally makes
                // every dispatch self-healing.
                call_session
                    .set_cancel_token(cancel.clone().unwrap_or_else(CancellationToken::new));
                let argv_owned = argv.to_vec();
                // Abort-on-drop: if this `run_command` future is cancelled (e.g.
                // an aborted background-job worker), abort the dispatch task too,
                // preserving the inline path's cancellation shape.
                let mut handle = AbortOnDrop(tokio::spawn(async move {
                    dispatch_command(command.as_ref(), &mut call_session, &argv_owned).await
                }));
                match (&mut handle.0).await {
                    Ok(result) => result,
                    // A panic inside the spawned dispatch surfaces as an engine
                    // command error rather than tearing the session down.
                    Err(join_err) => Err(EngineError::Command(CommandError::Other(format!(
                        "dispatch task failed: {join_err}"
                    )))),
                }
            }
            // Exclusive path: registry mutators (`load_template`/`unload`/`config`)
            // and unscoped fan-out hold the gate *exclusive* (no concurrent
            // readers), so dispatch directly against the canonical session — its
            // config/registry-structure mutations must persist for later calls.
            // The display is swapped for the call's own sink and restored after.
            //
            // Release the canonical session's per-call active guard afterwards:
            // `Command::run` re-installs a guard on the active entry as it returns,
            // and a guard lingering on the canonical session would block a later
            // *concurrent* forked call from locking that same entry (its
            // `try_lock_owned` in `activate` would fail). The MCP session holds no
            // active guard between calls; each call re-establishes its own.
            CommandLock::Exclusive(_) => {
                let mut session = self.session.lock().await;
                let prev_display = std::mem::replace(&mut session.display, call_display);
                // Install this dispatch's token on the canonical session: the
                // per-job token for a background worker, a fresh
                // (never-cancelled) one for a foreground call. Installing
                // unconditionally — instead of swap-and-restore — makes the
                // token state self-healing: if a hard-aborted worker skips the
                // restore below, the *next* dispatch's install wipes the stale
                // cancelled token before its pre-flight check.
                session.set_cancel_token(cancel.clone().unwrap_or_else(CancellationToken::new));
                let result = dispatch_argv(registry, &mut session, name, argv).await;
                // Best-effort tidy-up (skipped when the worker future is
                // dropped mid-dispatch; see the install note above).
                session.set_cancel_token(CancellationToken::new());
                session.display = prev_display;
                session.release_active_guard();
                result
            }
        };

        // Read this call's own buffer. The sink already bounded the output at
        // write time (discarding overflow before it was ever buffered). If it
        // dropped anything, append the same notice `cap_output` would — exactly
        // once, with the write-time overrun count. When nothing was dropped (or
        // the cap is disabled) the captured text is already within budget, so
        // `cap_output` is a no-op.
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
            // `--help`/`--version` is argparse-exit-0: return its text as a
            // success, not an error. clap renders help into the `Parse` message
            // (not the display sink), so surface that (capped); a genuine usage
            // error is exit 2 (below).
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
    /// When `sink` is `Some`, the whole dispatch (including the lock wait) is
    /// raced against a heartbeat that fires every `interval` via
    /// [`run_with_heartbeat`], so a slow foreground call does not time the client
    /// out. A `None` sink takes the original zero-overhead
    /// path — [`run_command`](Self::run_command) verbatim.
    ///
    /// # Errors
    ///
    /// Propagates [`McpCommandError`] from [`run_command`](Self::run_command)
    /// unchanged; the heartbeat path never alters the command's result.
    pub(crate) async fn run_command_with_progress(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
        sink: Option<&dyn ProgressSink>,
        interval: Duration,
    ) -> Result<String, McpCommandError> {
        match sink {
            None => self.run_command(registry, name, argv).await,
            Some(sink) => {
                run_with_heartbeat(self.run_command(registry, name, argv), sink, name, interval)
                    .await
            }
        }
    }

    /// Resolve the target RRIDs for a backgrounded fan-out, or `None` to keep
    /// the single-job path.
    ///
    /// Resolves `argv` exactly as the foreground dispatch does (via
    /// [`resolve_command_rrids`], which parses the command's own clap parser and applies its
    /// [`Scope`](mtui_core::Scope) against the loaded set), so the background
    /// fan-out matches the foreground one. Returns `None` when resolution is not
    /// meaningful (unparseable argv, or only the Null report resolves) — the
    /// caller then mints a single job whose body re-parses and runs as before.
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
    /// rejected as a whole (no partial spawn). Must be
    /// called while holding `jobs_guard` so the count and the subsequent inserts
    /// are atomic against a concurrent (http) spawn. `max_active_jobs == 0`
    /// disables the cap.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) naming the active/max counts when the spawn
    /// would exceed the cap.
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
    /// The worker runs through
    /// [`run_command`](Self::run_command) (so it takes the same per-RRID /
    /// registry gate and output cap as a foreground call) and records the
    /// terminal state/result on the job's `Arc<StdMutex<Job>>`; on settling it
    /// FIFO-evicts terminal records past the completed cap. `self` is an `Arc`
    /// because the spawned task must own the session for its `'static` lifetime.
    /// The caller holds the jobs lock so the admit-check and the insert are
    /// atomic against a concurrent spawn.
    ///
    /// `rrids` is the caller's already-computed template scope for `argv` (see
    /// [`Job::rrids`]); pass an empty vector when the caller could not resolve
    /// one.
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
                    // The cancel won the race and claimed the record, but a
                    // cooperative stop still produced a verdict naming what the
                    // flow managed to do (which packages were applied, how many
                    // templates ran). Record it so `job_result` can hand that
                    // back instead of a bare "was cancelled" — without
                    // rewriting the state the cancel already settled.
                    if let Err(err) = outcome {
                        j.error = Some(err.stderr);
                        if !err.stdout.is_empty() {
                            j.result = Some(err.stdout);
                        }
                    }
                }
            }
            // Bound retained history: evict oldest-finished terminal records.
            session.evict_completed();
        });
        job.lock().expect("job record poisoned").handle = Some(handle);
        job_id
    }

    /// FIFO-evict terminal job records beyond [`max_completed_jobs`](Self::max_completed_jobs).
    ///
    /// Keeps only the newest-`finished` terminal (done/failed/cancelled) records;
    /// running jobs are never evicted. `max_completed_jobs == 0` disables the cap.
    /// Runs under the jobs lock (never across an await).
    fn evict_completed(&self) {
        if self.max_completed_jobs == 0 {
            return;
        }
        let mut jobs = self.jobs.lock().expect("jobs table poisoned");
        // (finished-instant, id) for every terminal record.
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
        // Oldest first; drop everything past the newest `max_completed_jobs`.
        terminal.sort_by_key(|(finished, _)| *finished);
        let evict = terminal.len() - self.max_completed_jobs;
        for (_, id) in terminal.into_iter().take(evict) {
            jobs.remove(&id);
        }
    }

    /// Start `name`/`argv` in the background and return its job id.
    ///
    /// Mints exactly **one** job
    /// (id `"<command>-<n>"`) and returns immediately with a handle, so the
    /// client is not held for the minutes a `run`/`update`/`downgrade` can take.
    /// The tool layer calls [`start_jobs`](Self::start_jobs) instead so a
    /// fanned-out slow command yields one job per template; this stays the
    /// single-job primitive for tests and non-fan-out callers.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) when the session is already at
    /// `max_active_jobs` running jobs; no worker is
    /// spawned in that case.
    pub fn start_job(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
    ) -> Result<String, McpCommandError> {
        // Synchronous, so it cannot resolve the template scope (that needs the
        // session lock): the job records an empty scope and a forced cancel
        // falls back to every loaded template. See [`Job::rrids`].
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
    /// Resolves the target templates
    /// exactly as the foreground path does (via
    /// `resolve_job_rrids`). When more than one
    /// template resolves, mints **one job per template** — each running `argv`
    /// scoped to that template with `-T <rrid>` **prepended** (a positional
    /// `REMAINDER` command like `run` would otherwise swallow a trailing
    /// `-T <rrid>` into its own value) — so a backgrounded fanned-out slow
    /// command is independently observable and cancellable per template. When a
    /// single template (or none) resolves, this is exactly one job with the
    /// unchanged `<command>-<n>` id.
    ///
    /// The single-template job is `-T`-scoped too. Resolution happens at mint
    /// and dispatch happens later, so a `load_template` landing in between would
    /// otherwise turn an unscoped fan-out command into a two-template dispatch
    /// while the job record still named one — and a cancel would release only
    /// the recorded template's locks, stranding the other behind a
    /// success-shaped reply.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) when spawning the resolved jobs would breach
    /// `max_active_jobs`; the whole fan-out is rejected
    /// atomically (no partial spawn).
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
                // Admit or reject the whole batch atomically under the lock.
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
            // Exactly one real template: keep the single-job path (and its
            // stable id shape), but scope argv and record the RRID so the
            // dispatch cannot drift away from what the job says it targets.
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
            // Nothing real resolves (unparseable argv, unknown command, or only
            // the null report): there is no template to pin to or record.
            None => Ok(vec![self.start_job_scoped(
                registry,
                name,
                argv,
                Vec::new(),
            )?]),
        }
    }

    /// A poll-facing snapshot of one job record.
    ///
    /// `elapsed_s` is frozen at `finished` once terminal, else measured to now,
    /// rounded to 0.1s.
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
    /// [`McpCommandError`] when: the id is unknown; the job is still running
    /// (telling the caller to poll `job_status`); the job failed (carrying its
    /// captured stdout / error / exit code); or the job was cancelled.
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
            // Surface the cooperative stop's own verdict when the flow
            // produced one (which packages were applied, how far the fan-out
            // got); a forced abort has none, and keeps the bare form.
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
    /// Truthful, two-stage cancel:
    ///
    /// 1. **Cooperative:** the job's [`CancellationToken`] is cancelled first,
    ///    so a dispatch parked at a seam checkpoint (the `Command::run` driver's
    ///    pre-flight / between-templates checks, or a body observing
    ///    [`mtui_core::Session::cancel_requested`]) unwinds cleanly. The worker
    ///    gets `CANCEL_GRACE` to settle.
    /// 2. **Forced:** if the grace elapses (the body never checks the seam —
    ///    e.g. it is blocked mid host-op), the worker task is aborted. The
    ///    underlying SSH/subprocess operation may still run to completion on
    ///    the host — the same caveat as interrupting a foreground `run` — and
    ///    the reply says the abort was forced.
    ///
    /// A forced abort drops the dispatch's future mid-`await`, so the
    /// operation's own `unlock()` never runs and `/var/lock/mtui.lock` would be
    /// stranded on every host of every template the job was scoped to. The
    /// forced arm therefore releases it on the job's behalf and reports the
    /// per-host outcome in the reply. The cooperative arm does **not**: a body
    /// that unwound through its own flow ran its own unlock discipline.
    ///
    /// Only locks the job's **own** host group actually took are released, and
    /// never a comment-marked exclusive reservation — see
    /// [`HostsGroup::unlock_held`](mtui_hosts::HostsGroup::unlock_held).
    ///
    /// Releasing under this uncertainty matches what the command-timeout path
    /// already does (it unlocks unconditionally while documenting that the
    /// package manager may still hold its own lock). Where the aborted operation
    /// *is* a package transaction, the transaction itself stays serialised by
    /// the package manager's own system-wide lock; `/var/lock/mtui.lock` is
    /// mtui's coordination layer on top, and leaving it behind blocks every
    /// other tester on those hosts. A `run` or `reboot` body has no such
    /// second layer — for those, the release is purely mtui-side bookkeeping and
    /// the remote command may still be executing. Nothing else is done at the
    /// hosts — no reboot, no downgrade, no disconnect.
    ///
    /// A job already in a terminal state is **not** re-cancelled: the reply
    /// names its actual state instead of claiming a cancellation that never
    /// happened.
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
    /// [`close_with_timeout`](Self::close_with_timeout)'s does: the
    /// budget-expiry unit test bounds the wait to a fraction of a second
    /// instead of [`ABORT_UNLOCK_BUDGET`]. Both tests are colocated, so both
    /// seams stay private.
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
                    // here means the worker's terminal-write branch (which
                    // checks `Running`) can no longer overwrite it.
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
                // Await the aborted task so cancellation has fully unwound
                // before we return; a `JoinError::Cancelled` is expected.
                let _ = handle.await;
                // The worker is gone, so its `CommandLock` (registry-gate share
                // + per-RRID lock) is released and the fan-out below can take
                // those holds itself.
                unlocked = self.unlock_after_abort(&rrids, unlock_budget).await;
            }
            // The worker's terminal-write branch skipped its eviction (state
            // was already `Cancelled`), so reap history here.
            self.evict_completed();
        }
        if forced {
            // The prefix is a pinned contract (clients key on "forced abort");
            // the lock verdict goes inside the same parenthetical.
            let mut msg = format!(
                "cancelled job {job_id} (forced abort after {}s grace; a host \
                 operation already in flight may still finish on the host",
                CANCEL_GRACE.as_secs()
            );
            if let Some(clause) = unlocked.clause() {
                msg.push_str("; ");
                msg.push_str(&clause);
            }
            msg.push(')');
            Ok(msg)
        } else {
            Ok(format!("cancelled job {job_id}"))
        }
    }

    /// Releases the operation lock a force-aborted dispatch left behind, on
    /// every template in `rrids`.
    ///
    /// An empty `rrids` means the job recorded no scope (see [`Job::rrids`]) and
    /// falls back to every loaded template — the conservative reading for a
    /// dispatch that may have held the registry gate exclusively.
    ///
    /// The whole pass is bounded by `budget`, **including the preamble**: a
    /// template not reached in time is reported as unknown rather than waited
    /// out, so `job_cancel` stays responsive. Per template it does nothing but
    /// release the group's own held locks — no disconnect, no pool release, no
    /// history row.
    async fn unlock_after_abort(&self, rrids: &[String], budget: Duration) -> AbortUnlock {
        let mut summary = AbortUnlock::default();
        // The deadline is armed *before* the first await. The preamble below
        // takes the canonical session mutex, which an exclusive dispatch or a
        // `get`/`put` transfer holds for its whole duration — leaving it outside
        // the budget would let `job_cancel` block for minutes, which is exactly
        // what `ABORT_UNLOCK_BUDGET` promises it will not do.
        let deadline = Instant::now() + budget;

        // Preamble, deliberately **gate-free and unconditional**: drop the
        // active guard the aborted *exclusive* dispatch left on the canonical
        // session, then resolve the fallback scope.
        //
        // Doing this before (and independently of) the per-template holds is
        // load-bearing. That guard blocks `Session::activate`'s `try_lock_owned`
        // on the entry, so every later scoped dispatch on the template would
        // silently run against the null report — including the `list_locks` the
        // reply recommends, which would then report a false clean. If it were
        // only dropped inside the per-template pass, a busy gate (the RwGate is
        // writer-preferring, so one pending `load_template` blocks shared
        // acquisition) could burn the budget and leave the session poisoned.
        //
        // Gate-free is safe here, and `close_with_timeout` relies on the same
        // property: every writer of the canonical active guard either runs under
        // the exclusive gate or holds the session mutex for the whole write, so
        // taking the mutex is enough to make the drop atomic against them.
        let preamble = async {
            let mut session = self.session.lock().await;
            session.release_active_guard();
            if rrids.is_empty() {
                session.templates.rrids()
            } else {
                rrids.to_vec()
            }
        };
        let Ok(targets) = tokio::time::timeout(budget, preamble).await else {
            // Nothing is stranded by giving up here: the mutex being held that
            // long means a *live* dispatch owns it, and a live dispatch releases
            // its own active guard on the way out. The lingering-guard case (an
            // aborted exclusive job) leaves the mutex free, so this branch
            // cannot be that case.
            tracing::warn!(?budget, "post-abort unlock: session busy, release skipped");
            summary.stalled = true;
            return summary;
        };

        for rrid in targets {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.unlock_template(&rrid)).await {
                Ok(outcomes) => summary.absorb(outcomes),
                Err(_) => {
                    tracing::warn!(rrid = %rrid, ?budget, "post-abort unlock timed out");
                    summary.unknown.push(rrid);
                }
            }
        }
        summary
    }

    /// Releases one template's own held operation locks, taking the same holds a
    /// dispatch on it would.
    ///
    /// The gate-shared + per-RRID hold is not optional bookkeeping: it is what
    /// makes locking the report entry safe. [`Session::activate`] claims an
    /// entry with a *non-blocking* `try_lock_owned` and falls back to the null
    /// report when it fails, so a dispatch racing an entry lock this pass holds
    /// would silently act on nothing.
    ///
    /// Those holds shut out a same-RRID dispatch and any exclusive one, which is
    /// what this pass needs. They are not a crate-wide guarantee: `command_lock`
    /// resolves its scope and *then* acquires, and `close_with_timeout` locks
    /// entries gate-free — both pre-existing windows in which some other caller
    /// could still meet a locked entry.
    async fn unlock_template(&self, rrid: &str) -> BTreeMap<String, LockOutcome> {
        // Same acquire order as `command_lock` (gate-shared → one rrid lock),
        // and only ever one rrid lock at a time, so this cannot deadlock
        // against a concurrent dispatch.
        let _shared = self.gate.shared().await;
        let rrid_lock = self.lock_for(rrid);
        let _rrid = rrid_lock.lock_owned().await;

        // The preamble already dropped any guard the aborted dispatch left, so
        // this only needs the handle.
        let entry = self.session.lock().await.templates.handle(rrid);
        // Unloaded since the job was minted: nothing of ours to release.
        let Some(entry) = entry else {
            return BTreeMap::new();
        };
        // The scoped path's inner dispatch task is aborted asynchronously, so it
        // may still hold this entry for a moment; the caller's budget bounds the
        // wait.
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

    /// Each session gets a distinct id, and a session's id is stable across
    /// calls — the freshness invariant `remint_after_drop_is_a_new_session`
    /// relies on instead of `Arc` address identity.
    #[test]
    fn session_id_is_unique_and_stable() {
        let a = McpSession::new(Config::default());
        let b = McpSession::new(Config::default());
        assert_ne!(a.id(), b.id(), "distinct sessions must have distinct ids");
        assert_eq!(a.id(), a.id(), "a session's id is stable across calls");
    }

    /// A host whose `close()` never returns must not block `close_with_timeout`.
    ///
    /// With a
    /// small budget, teardown returns despite the stuck close, the healthy host
    /// is still closed, and the abandoned close is later released so its task
    /// unwinds. Bounding via [`tokio::time::timeout`] is the whole point — see
    /// the module docs.
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

        // A generous outer guard: the fix returns in ~0.2s; a regression that
        // waited on the wedged close would hit this and fail loudly.
        let bounded = tokio::time::timeout(
            Duration::from_secs(15),
            sess.close_with_timeout(Duration::from_millis(200)),
        )
        .await;
        assert!(bounded.is_ok(), "close_with_timeout did not return in time");

        // The healthy host was closed even though a sibling close hung.
        assert!(
            good.is_closed(),
            "healthy host closed despite wedged sibling"
        );

        // Release the abandoned close so its task unwinds and does not linger.
        gate.notify_waiters();
    }

    /// A fresh session honours the non-interactive contract: no prompter is
    /// wired; `interactive = false` is provided by
    /// `capture::session` passing `is_repl = false`.
    #[tokio::test]
    async fn new_session_is_non_interactive() {
        let sess = session(Config::default());
        let guard = sess.session().lock().await;
        assert!(
            guard.prompter().is_none(),
            "MCP session must have no prompter"
        );
    }

    /// The happy path: `whoami` returns the same banner the REPL prints, routed
    /// through the shared engine.
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

    /// An unknown flag is a parse failure: `McpCommandError` with exit 2 and the
    /// offending token surfaced in stderr.
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

    /// An unknown command maps to exit 1 (not a parse error).
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

    /// `--help` is argparse-exit-0: it returns the help text as a success rather
    /// than an error envelope.
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

    /// A tiny configured cap truncates the tool result and appends the notice.
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

    /// A command that emits far more than the cap is bounded at *write time*:
    /// the result is truncated to the budget, carries exactly one notice, and
    /// reports the correct budget-overrun count — proving the full payload was
    /// never buffered (it was discarded as it was written).
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
        // Head kept up to the budget, then a single notice.
        assert!(
            out.starts_with(&"x".repeat(cap)),
            "head kept: {}",
            &out[..40]
        );
        assert_eq!(out.matches("truncated").count(), 1, "exactly one notice");
        // Overrun = total - limit.
        assert!(
            out.contains(&format!("truncated {} bytes", total - cap)),
            "correct dropped count: {out}"
        );
        assert!(out.contains(&format!("max_output_bytes={cap}")));
    }

    /// Each call isolates its own output: a second call does not see the first
    /// call's captured text.
    #[tokio::test]
    async fn run_command_isolates_output_per_call() {
        let mut config = Config::default();
        config.session_user = "alice".to_owned();
        let sess = session(config);
        let registry = register_all();

        let first = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        let second = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        // Identical, single-banner output — not the first call's text doubled.
        assert_eq!(first, second);
        assert_eq!(
            second.matches("User: alice").count(),
            1,
            "no bleed: {second:?}"
        );
    }

    /// `McpCommandError` renders a one-line summary plus stderr.
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

    /// Eviction FIFO-drops the oldest terminal records to the completed cap and
    /// never removes a still-running record, even under cap pressure.
    ///
    /// Driven directly against the private jobs table (fabricated records) so the
    /// invariant is deterministic — an integration test cannot force a concurrent
    /// completion while another job holds the single session mutex.
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
            // Three terminal records with increasing finish times + one running.
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
        // Oldest terminal (t-1) evicted; newest two terminals kept; running kept.
        assert!(!ids.contains("t-1"), "oldest terminal evicted: {ids:?}");
        assert!(ids.contains("t-2"), "kept: {ids:?}");
        assert!(ids.contains("t-3"), "kept: {ids:?}");
        assert!(ids.contains("run"), "running never evicted: {ids:?}");
        assert_eq!(ids.len(), 3);
    }

    /// A zero completed cap disables eviction (records accumulate).
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

    /// `lock_for` returns the *same* lock object for a repeated RRID (so
    /// same-RRID calls contend) and a *different* one for a distinct RRID.
    #[test]
    fn lock_for_shares_per_rrid() {
        let sess = session(Config::default());
        let a1 = sess.lock_for("SUSE:Maintenance:1:1");
        let a2 = sess.lock_for("SUSE:Maintenance:1:1");
        let b = sess.lock_for("SUSE:Maintenance:2:1");
        assert!(Arc::ptr_eq(&a1, &a2), "same RRID shares one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "distinct RRIDs get distinct locks");
    }

    /// An unknown command resolves to no RRID → `command_lock` takes the gate
    /// exclusively.
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

    /// A self-scoped single-shot command with nothing loaded resolves to the
    /// null report only → exclusive gate (unscoped fallback).
    #[tokio::test]
    async fn command_lock_unscoped_is_exclusive() {
        let sess = session(Config::default());
        let registry = register_all();
        // `whoami` is `Scope::Active`; with nothing loaded it resolves to the
        // empty null RRID, which `resolve_command_rrids` drops → None → exclusive.
        let lock = sess.command_lock(&registry, "whoami", &[]).await;
        assert!(matches!(lock, CommandLock::Exclusive(_)));
    }

    /// `scoped_lock(None)` with nothing loaded falls back to the active (empty)
    /// RRID and yields a scoped hold without deadlocking.
    #[tokio::test]
    async fn scoped_lock_falls_back_to_active() {
        let sess = session(Config::default());
        let lock = sess.scoped_lock(None).await;
        assert!(matches!(lock, CommandLock::Scoped { .. }));
    }

    /// A registry-structure mutator (`load_template`) takes the gate *exclusive*
    /// even when a single template is loaded (so its `resolve_command_rrids`
    /// would otherwise be a single RRID). Guards the `mutates_registry` routing
    /// added for the concurrent dispatch path: a structural mutation must land on the
    /// canonical session, not a discarded per-call fork. A content command scoped
    /// to that same template still takes the *scoped* (concurrent) path.
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

        // `load_template` mutates the registry → exclusive, despite one template
        // being loaded/active.
        let mutator = sess
            .command_lock(&registry, "load_template", &[rrid.to_owned()])
            .await;
        assert!(
            matches!(mutator, CommandLock::Exclusive(_)),
            "registry mutator must take the exclusive gate"
        );
        drop(mutator);

        // A content command scoped to the same single template still takes the
        // concurrent (scoped) path.
        let scoped = sess
            .command_lock(&registry, "list_hosts", &["-T".to_owned(), rrid.to_owned()])
            .await;
        assert!(
            matches!(scoped, CommandLock::Scoped { .. }),
            "content command on one template stays on the scoped path"
        );
    }

    /// Cancelling a *finished* job is a no-op that still reports success (the
    /// non-running branch of `job_cancel`), and does not rewrite its state.
    #[tokio::test]
    async fn job_cancel_finished_job_is_noop() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let registry = Arc::new(register_all());

        let job_id = sess
            .start_job(Arc::clone(&registry), "whoami", Vec::new())
            .expect("start_job succeeds");
        // Drive it to completion.
        for _ in 0..500 {
            if sess.job_status(&job_id).unwrap().state != JobState::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Done);

        let msg = sess.job_cancel(&job_id).await.expect("cancel is a no-op");
        // Truthful no-op: the reply names the actual terminal state instead of
        // claiming a cancellation that never happened.
        assert_eq!(msg, format!("job {job_id} already done; nothing to cancel"));
        // State is unchanged: a finished job is not rewritten to Cancelled.
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Done);
        // The result is preserved and still retrievable.
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

        // Signals the test once the body is parked on the seam, so the cancel
        // is issued only after the dispatch is genuinely mid-flight.
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
                // Park on the seam: unwind the moment job_cancel fires the
                // token, well inside CANCEL_GRACE.
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
        // Cooperative: the body observed the token, so no forced abort — and
        // the whole cancel settles well inside the grace window.
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
        // The cancelled record still reports the standard envelope on read.
        let err = sess.job_result(&job_id).expect_err("cancelled job raises");
        assert!(err.stderr.contains("was cancelled"), "got: {err:?}");

        // Self-healing end-to-end: the hard abort skipped the exclusive path's
        // token restore, leaving the cancelled job token on the canonical
        // session — the next dispatch must install a fresh token before its
        // pre-flight check and therefore succeed, not report "cancelled".
        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("dispatch after a forced abort must not see a stale cancelled token");
        assert!(out.contains("testuser"), "got: {out}");
    }

    /// The truthful no-op replies for the two other terminal states: a failed
    /// and an already-cancelled job each name their actual state.
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
            // The record's state is not rewritten.
            assert_eq!(sess.job_status(id).unwrap().state, state);
        }
    }

    /// `job_result` on a cancelled job surfaces the "was cancelled" envelope.
    #[tokio::test]
    async fn job_result_cancelled_job_raises() {
        let sess = session(Config::default());
        // Seed a cancelled record directly (no worker needed for this read path).
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

    /// The RRIDs the lock tests load.
    const LOCK_RRID_A: &str = "SUSE:Maintenance:1:1";
    const LOCK_RRID_B: &str = "SUSE:Maintenance:2:1";

    /// Loads `rrid` with a host group over `mocks` (keyed by the given names)
    /// into `sess` and makes it active.
    ///
    /// The mock handles stay `Arc`-shared with the ones the caller keeps, so
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

    /// Blocks until `mock` records the exclusive lockfile create.
    ///
    /// Anti-vacuity guard: without it a cancel could land before the dispatch
    /// ever locked, and "the lock was released" would pass against a host that
    /// was never locked in the first place.
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
    /// re-stamp `lock()` performs over a lock this process already holds.
    ///
    /// The anti-vacuity anchor when the host was already locked before the job
    /// started: `await_locked` would be satisfied by the *earlier* exclusive
    /// create and prove nothing about the job.
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

    /// A lockfile line stamped with **this process's** identity.
    ///
    /// What a sibling template's — or another MCP session's — live hold looks
    /// like on a shared refhost: wire ownership is per user + PID, so it reads
    /// back as "mine" to every host group in this process.
    /// `Target::with_connection` builds its lock from `Config::default()`, so
    /// that is the identity to match.
    fn ours_lockfile() -> Vec<u8> {
        format!(
            "1700000000:{}:{}",
            Config::default().session_user,
            std::process::id()
        )
        .into_bytes()
    }

    /// A fan-out probe that takes the group's operation lock and then parks.
    ///
    /// `park` decides how: [`Park::Forever`] never observes the cancellation
    /// seam (so the cancel must force-abort it, stranding the lock — the #405
    /// shape), [`Park::Seam`] unwinds cooperatively, and [`Park::Gate`] waits for
    /// a test-issued permit and then runs its own `unlock()` (a well-behaved
    /// concurrent job).
    ///
    /// The gate is a [`Semaphore`](tokio::sync::Semaphore), not a `Notify`: a
    /// permit is *stored*, so a release issued before the body reaches the await
    /// is still observed. `Notify::notify_waiters` would be lost in that window
    /// and the test would hang.
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
        /// A probe that locks the whole group with no comment (an ordinary
        /// operation hold) and parks per `park`.
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
                    // hard abort stops it — and its `unlock()` never runs.
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

    /// Registry carrying the lock probe plus the real commands.
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
    /// its record names.
    ///
    /// Resolution happens at mint and dispatch happens later, so an unpinned
    /// argv lets a `load_template` in between turn the dispatch into a wider
    /// fan-out than the record describes — and a cancel would then release only
    /// the recorded template's locks while reporting success.
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

    /// #405 headline: a job force-aborted mid host-operation has its operation
    /// lock released on every host of the template it was scoped to, and the
    /// reply names them.
    ///
    /// Driven through the real `run` command (`lock_selected` → `targets.run` →
    /// `unlock_selected`, with no cancellation checkpoint in between), which is
    /// the faithful shape of the bug: the abort drops the future between the
    /// lock and the unlock.
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

        // `start_job` records no template scope, and an unscoped fan-out over
        // two templates takes the gate exclusively — the inline dispatch path.
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

        // The canonical session must hold no active guard afterwards: a scoped
        // dispatch forks and claims the entry with a *non-blocking*
        // `try_lock_owned`, so a lingering guard would not error — it would
        // silently list the null report's (empty) host set.
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

    /// The release is scoped to the cancelled job's own templates: a sibling
    /// job still mid-operation on another template keeps its locks, and the
    /// cancel does not queue behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forced_cancel_leaves_a_concurrent_templates_locks_alone() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        // Both templates' bodies lock and park; `Gate` lets the test release
        // them, and B is never released before the cancel.
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
        // The semantic pin, independent of timing: the other template must not
        // appear in the verdict in any bucket.
        assert!(
            !msg.contains(LOCK_RRID_B),
            "the reply must not mention another template at all: {msg}"
        );
        assert!(
            !saw_unlock(&beta),
            "the cancel released a lock belonging to a live job on another template"
        );
        // A release that fell back to every loaded template would queue behind
        // job B's per-RRID lock and burn the whole budget instead.
        assert!(
            elapsed < CANCEL_GRACE + Duration::from_secs(3),
            "cancel waited on the other template's lock: {elapsed:?}"
        );

        // Job B was genuinely mid-operation holding its lock the whole time:
        // released, it finishes normally and runs its own unlock.
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
    ///   remote line belongs to someone else (a reboot cleared `/var/lock` and
    ///   another tester claimed it, a stale reap fired): benign contention.
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

        // Our hold on host-stolen is taken over after we acquired it. `with_file`
        // writes through the mock's `Arc`-shared file table, so this lands on the
        // clone the host group owns.
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

    /// The release is bounded: a host whose lock read outruns the budget is
    /// reported as unknown (with the scoped remedy) instead of blocking the
    /// cancel, and the reply does not claim the lock is gone.
    ///
    /// The budget is set **above** the preamble/gate/entry acquisition cost and
    /// **below** the mock's per-SFTP-read delay, so the expiry is attributable
    /// to the host fan-out itself rather than to lock-acquisition noise; the
    /// elapsed assertion is tightened to match.
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
        // Grace + budget + slack. Loose enough not to flake, tight enough that
        // waiting out even one 400ms host read would breach it.
        assert!(
            elapsed < CANCEL_GRACE + budget + Duration::from_millis(700),
            "the release was not bounded by the budget: {elapsed:?}"
        );
        // And that is the truth: the lock really is still there.
        assert!(!saw_unlock(&slow), "the lockfile removal was reached");
        assert!(still_locked(&slow), "host-slow's lock is gone after all");
    }

    /// The release belongs to the *forced* arm only.
    ///
    /// The probe deliberately unwinds through the seam **without** unlocking, so
    /// the surviving lock is unambiguous evidence that the abort-path release
    /// did not run — the pin is "forced-only", not "the cooperative body cleaned
    /// up". (A real cooperative flow owns its own unlock discipline; that is why
    /// the cancel does not second-guess it.) The reply must also stay
    /// byte-identical.
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

    /// A same-process lock this group never took is left alone.
    ///
    /// The shape: a refhost shared with another loaded template (or another MCP
    /// session in the same server) that is *live*-locked by its owner. Wire
    /// ownership is per-PID, so the lock reads back as "mine" — a whole-group
    /// `unlock()` would remove a sibling's hold mid-transaction and report it as
    /// released. Scoping on what this group's own `Target` objects actually took
    /// is what separates them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_leaves_a_same_process_lock_this_group_never_took() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));
        // Seeded with *our own* user+pid: exactly what a sibling template's live
        // job leaves on a shared refhost.
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
        // host-beta was never locked, so it is neither acted on nor claimed as
        // "unlocked" — a whole-group release would report it either way.
        assert!(
            !msg.contains("host-beta"),
            "a host the job never locked must not appear in the verdict: {msg}"
        );
        assert!(!saw_unlock(&beta), "host-beta was acted on");
    }

    /// An operator's `lock <comment>` reservation survives the cancel.
    ///
    /// A non-empty comment marks an **exclusive** hold — the PI assignment lock
    /// the session re-applies on every connect and after every reboot, or a
    /// deliberate manual reservation. Operation flows all stamp an empty
    /// comment, so the cancel releases those and leaves the reservation alone.
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

        // The real `lock` command: whole-group, carrying a comment (`-c` is what
        // its own docs call "keeps the lock effective against other sessions").
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
        // and hangs; host-reserved keeps the marked reservation. Anchor on the
        // *re-stamp*: the exclusive create already happened above.
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

    /// A client-supplied `-T` narrowing to one template is recorded as that
    /// template, so the cancel never reaches for the other loaded one.
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

    /// The lingering active guard is dropped even when the release never gets
    /// to run a single template.
    ///
    /// The guard is dropped in a bounded preamble, before any gate or per-RRID
    /// wait, precisely so a busy gate cannot leave the session poisoned. Here
    /// the per-template pass is blocked on the template's own dispatch lock for
    /// the whole budget, so a release that only dropped the guard *inside* the
    /// per-template body would never drop it at all — and every later scoped
    /// dispatch on that template would silently run against the null report,
    /// including the `list_locks` the reply recommends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_clears_the_active_guard_even_when_the_release_cannot_run() {
        let alpha = MockConnection::new("host-alpha");
        let beta = MockConnection::new("host-beta");

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        load_with_hosts(&sess, LOCK_RRID_B, &[("host-beta", beta.clone())]).await;

        // Unscoped fan-out over two templates: the exclusive, inline dispatch
        // path, which leaves its active guard on the canonical session when
        // aborted. It parks on A, so the guard is A's.
        let registry = registry_with_probe(LockAndPark::parking_on(Park::Forever, LOCK_RRID_A));
        let job_id = sess
            .start_job(Arc::clone(&registry), "lock_and_park_probe", Vec::new())
            .expect("start_job succeeds");
        await_locked(&alpha, "host-alpha").await;

        // Block *both* templates' dispatch locks for the whole cancel, so the
        // per-template body is never entered for either. Taken directly (not via
        // `scoped_lock`, which would first queue on the gate behind the job's
        // exclusive hold), so the holds are established before the cancel with
        // no ordering race.
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
    /// exclusive dispatch or a `get`/`put` transfer must not make `job_cancel`
    /// block for that operation's whole duration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_cancel_does_not_block_on_a_busy_session() {
        let alpha = MockConnection::new("host-alpha").with_run_delay(Duration::from_secs(600));

        let sess = session(Config::default());
        load_with_hosts(&sess, LOCK_RRID_A, &[("host-alpha", alpha.clone())]).await;
        let registry = Arc::new(register_all());

        // The scoped dispatch path touches the canonical session only briefly
        // (to fork), so the test can hold the session mutex while the job is
        // parked mid host-operation.
        let ids = sess
            .start_jobs(Arc::clone(&registry), "run", vec!["true".to_owned()])
            .await
            .expect("start_jobs succeeds");
        await_locked(&alpha, "host-alpha").await;
        let busy = sess.session().lock().await;

        let budget = Duration::from_millis(200);
        let before = Instant::now();
        // An unbounded preamble would wait for `busy` forever, so bound the call
        // itself: the failure must read as "job_cancel blocked", not as a hung
        // test.
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

        // `start_job` records no scope, so the release falls back to both
        // loaded templates and walks them in registry order. The probe parks
        // only on B, so A locks and returns and *both* groups end up holding.
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

    /// A job scoped to a template that is no longer loaded has nothing of ours
    /// to release, and a release with nothing to report leaves the forced reply
    /// byte-identical to what it has always been.
    #[tokio::test]
    async fn forced_cancel_with_nothing_to_release_keeps_the_reply_unchanged() {
        let sess = session(Config::default());
        // A worker that never settles, recorded against a template the session
        // never loaded.
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

    // ---- progress heartbeats ----------------------------------------------- //

    /// Records every frame `report` receives.
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

    /// Records the attempt then "fails" — but a `ProgressSink` swallows its own
    /// transport errors, so from the loop's view this is indistinguishable from a
    /// working sink. This lets us assert
    /// the command result survives even when the sink's send would have failed.
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
                // The real rmcp sink logs at DEBUG and swallows a send error here;
                // model that by simply not propagating anything.
            })
        }
    }

    /// `sink = None` takes the zero-overhead path: no frames, same stdout as a
    /// bare `run_command`.
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
        // The sink we built was never passed, so it recorded nothing.
        assert!(sink.calls().is_empty(), "no frames on the None path");
    }

    /// A slow future with a small interval fires >= 1 monotonic frame, each
    /// carrying the command name; the future's output is returned unchanged.
    /// Driven directly over a controlled sleep to keep the timing deterministic.
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

    /// A future that finishes well inside the interval fires zero frames.
    #[tokio::test]
    async fn run_with_heartbeat_no_frames_for_fast_future() {
        let sink = RecordingSink::default();
        let out = run_with_heartbeat(async { 7 }, &sink, "fast", Duration::from_secs(1)).await;
        assert_eq!(out, 7);
        assert!(sink.calls().is_empty(), "no frames: {:?}", sink.calls());
    }

    /// A failing command surfaces `McpCommandError` unchanged through the
    /// heartbeat path.
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

    /// A sink whose send would fail must not mask the command result: the slow
    /// future still returns its value and the sink's attempts are recorded.
    ///
    /// Driven on a paused clock so the schedule is exact rather than a race
    /// against wall time: tokio auto-advances to each next timer, so the ticks
    /// land at virtual 40/80/120ms and the body completes at 150ms — three
    /// attempted sends, every run. The wall-clock version could observe zero
    /// ticks on a starved CPU.
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
