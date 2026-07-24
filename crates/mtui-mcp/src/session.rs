//! Per-client MCP session.
//!
//! [`McpSession`] is the headless mtui session that backs one `mtui-mcp` client.
//! It is the Rust analogue of upstream `mtui.mcp.session.McpSession`: it owns the
//! mutable [`Session`] state a command dispatches against plus the [`SharedBuf`]
//! sink that captures the command's display output for the tool result, and
//! exposes [`run_command`](McpSession::run_command) — the central dispatch
//! primitive the tool layer calls (drain → dispatch → capture → output-cap).
//!
//! Under **stdio** one instance serves the single client; under **http** the
//! future `SessionRegistry` (P7.10) owns one instance per client. In both cases
//! the [`crate::provider::SessionProvider`] seam hands callers an
//! `Arc<McpSession>`, so the tool layer (P7.6/P7.8) stays transport-agnostic.
//!
//! ## Scope (landed vs deferred)
//!
//! P7.3 landed the dispatch primitive: `run_command`, the [`McpCommandError`]
//! failure envelope, and the per-result output cap (`[mcp] max_output_bytes`).
//! The non-interactive contract (`interactive = false`, unset prompter) is
//! already provided by [`capture::session`] passing `is_repl = false`.
//!
//! P7.3a (`mtui-rs-76e.11`) landed the per-template **lock discipline**: a
//! shared/exclusive registry gate ([`crate::concurrency::RwGate`], upstream
//! `_RWLock`) plus a lazily-created per-RRID lock map (upstream `_rrid_locks`).
//! [`command_lock`](McpSession::command_lock) takes the gate *shared* + one
//! per-RRID lock for a single-template call (so same-RRID calls serialise and
//! different-RRID calls take distinct locks) and the gate *exclusive* for
//! fan-out / registry mutators; [`scoped_lock`](McpSession::scoped_lock) is the
//! same hold for the hand-written testreport tools.
//!
//! **Landed** (`mtui-rs-f36r` / `mtui-rs-0mop.11`) — genuine wall-clock
//! concurrency between *different-RRID* calls plus per-call output isolation. A
//! single-real-RRID call dispatches on a [`Session::fork_for_call`] (which shares
//! the loaded reports' per-entry `Arc<Mutex<..>>` locks and carries its own
//! display) via [`dispatch_command`], spawned so it overlaps a concurrent
//! different-RRID call; [`run_command`](McpSession::run_command) no longer holds
//! a session-wide mutex across the scoped dispatch. Registry-structure mutators
//! ([`Command::mutates_registry`](mtui_core::Command::mutates_registry)) and
//! unscoped fan-out still take the gate *exclusive* against the canonical
//! session. All four `tests/session_concurrency.rs` parity tests pass.
//!
//! P7.3b (`mtui-rs-76e.12`) landed the **background-job table** (`_jobs`): a slow
//! `run`/`update`/`downgrade` can be started with
//! [`start_jobs`](McpSession::start_jobs) (one job per resolved template, each
//! `-T <rrid>`-scoped) and returns a handle immediately instead of holding the
//! request open; the outcome is polled via
//! [`job_status`](McpSession::job_status) / [`job_result`](McpSession::job_result)
//! and controlled via [`job_list`](McpSession::job_list) /
//! [`job_cancel`](McpSession::job_cancel). Each job worker runs through the same
//! [`run_command`](McpSession::run_command) primitive (so it takes the same
//! per-RRID / registry gate and output cap as a foreground call).
//!
//! Bead `mtui-rs-th4o.8` bounded this table's resource use: a spawn is rejected
//! (before allocating a worker) once the session holds
//! `[mcp] max_active_jobs` running jobs — a fan-out is admitted or rejected as a
//! whole — and terminal records are FIFO-evicted to `[mcp] max_completed_jobs`
//! so a long-lived session does not accumulate job history unbounded (`0`
//! disables either cap). The capture sink is likewise bounded at *write time*
//! (see [`crate::capture`]) so a single command cannot buffer more than
//! `[mcp] max_output_bytes` before the cap applies.
//!
//! P7.3d (`mtui-rs-76e.14`) landed the `notifications/progress` **heartbeats**:
//! a long-running foreground tool call ([`run_command_with_progress`]) races the
//! dispatch against a ticker that emits a progress frame every
//! [`DEFAULT_PROGRESS_INTERVAL`] against a transport-free [`ProgressSink`], so an
//! MCP client that honours the protocol's progress contract does not time out on
//! `run`/`update`/`set_repo`/`commit`. The rmcp-backed sink (peer +
//! `progressToken`) is built in [`crate::server`] from the request context; this
//! layer stays rmcp-free. A `None` sink takes the original zero-overhead path.
//!
//! P7.3c (`mtui-rs-76e.13`) landed [`close`](McpSession::close): the session
//! teardown the http `SessionRegistry` (P7.10 / `mtui-rs-odq8`) calls on
//! eviction. For **every** loaded template it releases the report's pool claims
//! then disconnects its host group, best-effort + idempotent, under a bounded
//! [`DISCONNECT_TIMEOUT`] so a wedged host close cannot block the idle-sweep.
//! Unlike upstream it does not empty each `HostsGroup` (a closed `Target` is left
//! in the group with a dead connection, dropped whole with the report) and it
//! bounds the wait with [`tokio::time::timeout`] rather than a thread-pool
//! `shutdown(wait=False)` — the Python machinery existed only to defeat
//! `Executor.__exit__`'s blocking join, which Rust has no equivalent of.
//!
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use mtui_config::Config;
use mtui_core::{
    ColorMode, CommandError, CommandPromptDisplay, EngineError, Registry, Session, dispatch_argv,
    dispatch_command, resolve_command_rrids,
};
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::task::JoinHandle;

use crate::capture::{self, SharedBuf};
use crate::concurrency::{ExclusiveGuard, RwGate, SharedGuard};
use crate::slim::{cap_output, truncation_notice};

/// Wall-clock budget for the whole [`close`](McpSession::close) host-disconnect
/// fan-out (upstream `DISCONNECT_TIMEOUT_SECONDS = 45.0`).
///
/// A wedged host teardown (a dead peer with no RST whose close never returns)
/// must not block the http registry's idle-sweep behind it; a close that
/// overruns this bound is logged and abandoned so `close()` always returns.
pub(crate) const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(45);

/// Default interval between `notifications/progress` heartbeat frames while a
/// long-running foreground tool call runs (upstream
/// `DEFAULT_PROGRESS_INTERVAL_SECONDS = 10.0`).
///
/// Not a config key (upstream has none): it is the default the tool layer passes
/// to [`McpSession::run_command_with_progress`], overridable per call so tests
/// can drive a sub-second interval.
pub(crate) const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

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
/// The Rust analogue of the single `ctx.report_progress` coroutine upstream's
/// `_run_with_heartbeat` consumes. Keeping it a trait (rather than importing the
/// rmcp `Peer`) keeps this crate's session layer transport-free and unit-testable
/// with a recording double; the rmcp-backed implementation
/// (`crate::server::PeerProgressSink`) is built from the request context and sends
/// a real `notifications/progress`.
///
/// Implementors **must not** propagate transport failures: a send error is the
/// concern of the sink (log at DEBUG and swallow) so a flaky client can never mask
/// the command's actual outcome (upstream swallows `ctx.report_progress`
/// exceptions in the loop).
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
/// The Rust analogue of upstream `McpSession._run_with_heartbeat`: instead of a
/// worker thread raced with `asyncio.wait`, `fut` is already async so we
/// [`tokio::select!`] it against a ticker. Each tick reports the elapsed seconds
/// and a `"<command> running (<n>s)…"` message — byte-for-byte the frame shape
/// upstream emits. Progress values are monotonic (elapsed since start). When `fut`
/// completes first its output is returned unchanged; a heartbeat is never emitted
/// after completion.
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
/// The Rust analogue of upstream `mtui.mcp.session.McpCommandError`: it carries
/// the streams captured during the failed run so the server layer can surface
/// them to the client:
///
/// * `stdout` — everything the command printed before failing (already capped).
/// * `stderr` — the parse/usage complaint or the command-error message.
/// * `exit_code` — argparse-style status: `2` for a usage/parse error, `1` for
///   an unknown command or a command-body failure.
///
/// [`Display`](std::fmt::Display) renders a one-line summary plus the captured
/// stderr so the default MCP error envelope is human-readable (mirrors
/// upstream's `_render`).
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
/// The Rust analogue of upstream's `job["state"]` string
/// (`running`/`done`/`failed`/`cancelled`); [`Display`](std::fmt::Display)
/// renders the same lowercase token the job tools print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// The worker task is still executing (or queued behind its lock).
    Running,
    /// The command finished successfully; its stdout is in [`Job::result`].
    Done,
    /// The command failed; [`Job::error`]/[`Job::exit_code`] carry the envelope.
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

/// One background-job record (upstream `_jobs[job_id]` dict).
///
/// Shared between the spawned worker (which writes the terminal state/result)
/// and the poll methods (which read it), so it lives behind an
/// `Arc<StdMutex<Job>>` in [`McpSession::jobs`]. The `StdMutex` is only ever
/// held for a field read/write, never across an `.await`.
#[derive(Debug)]
struct Job {
    /// The session-unique job id (`"<command>-<n>"` or `"<command>-<rrid>-<n>"`).
    id: String,
    /// The command name (upstream `cmd_cls.command`).
    command: String,
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
    /// The worker task handle, aborted by [`McpSession::job_cancel`].
    handle: Option<JoinHandle<()>>,
}

/// A public, poll-facing snapshot of a [`Job`] (no task handle).
///
/// The Rust analogue of upstream `_job_view`; the job tools render it into the
/// one-line status text.
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

/// Process-global monotonic source of [`McpSession::id`] values. Each session
/// pulls a fresh id at construction, so two distinct sessions never share one
/// (freshness independent of heap-address reuse — bead `mtui-rs-1edj`).
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// A headless mtui session backing one MCP client.
///
/// Holds the [`Session`] behind a [`Mutex`] because command dispatch
/// ([`mtui_core::dispatch_argv`]) needs `&mut Session` while the rmcp
/// `ServerHandler` methods take `&self` (P7.1 spike finding). The paired
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
    /// The registry shared/exclusive gate (upstream `_RWLock` `_registry`).
    ///
    /// A command scoped to exactly one template enters this in *shared* mode
    /// (so it cannot overlap a registry mutation); registry mutators
    /// (`load_template`/`unload`) and unscoped fan-out enter it *exclusive*,
    /// draining in-flight per-RRID work. See [`command_lock`](Self::command_lock).
    gate: RwGate,
    /// Lazily-created per-RRID locks (upstream `_rrid_locks` + `_locks_guard`).
    ///
    /// Same-RRID calls share one `Arc<Mutex<()>>` and serialise; different-RRID
    /// calls take different locks. The outer [`StdMutex`] guards the map's own
    /// lazy population (held only for the get-or-insert, never across an await).
    rrid_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The background-job table (upstream `_jobs`), keyed by job id.
    ///
    /// A backgrounded slow command runs in a spawned worker that records its
    /// outcome on its `Arc<StdMutex<Job>>`; the poll methods
    /// ([`job_status`](Self::job_status) / [`job_result`](Self::job_result))
    /// read it without locking the session. Under http the registry's idle
    /// sweep drops the whole session and its table with it; within a session's
    /// lifetime the table is **bounded** — active spawns are capped by
    /// [`max_active_jobs`](Self::max_active_jobs) and terminal records are
    /// FIFO-evicted to [`max_completed_jobs`](Self::max_completed_jobs) (bead
    /// `mtui-rs-th4o.8`). The outer [`StdMutex`] guards insert/lookup/eviction
    /// only (never held across an await).
    jobs: StdMutex<HashMap<String, Arc<StdMutex<Job>>>>,
    /// Monotonic job-id counter (upstream `_job_counter`), pre-incremented per
    /// minted job so ids are session-unique.
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
/// Returned by [`McpSession::command_lock`] / [`McpSession::scoped_lock`] and
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
    /// [`capture::session`].
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
    /// [`cap_output`](crate::slim::cap_output) budget `run_command` applies.
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
    /// tasks racing to lock the same fresh RRID share one lock object. The Rust
    /// analogue of upstream `_lock_for`.
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
    /// The Rust analogue of upstream `_command_lock`, resolving exactly as the
    /// foreground dispatch does (via [`resolve_command_rrids`]):
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
            // the exclusive gate (`mtui-rs-f36r`, steps 4-5).
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

    /// Holds the registry-shared gate plus one template's per-RRID lock.
    ///
    /// For the hand-written testreport tools (which act on a single template's
    /// files): entering the gate *shared* keeps the loaded set stable for the
    /// body (no concurrent `load_template`/`unload`) while still letting tools on
    /// *other* templates run in parallel, and the per-RRID lock serialises
    /// against foreground dispatch for the *same* template (e.g. a concurrent
    /// `commit`). The Rust analogue of upstream `scoped_lock`.
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
    /// The Rust analogue of upstream `McpSession.close` /
    /// `McpSession._disconnect_targets`. Owned by the http
    /// `SessionRegistry` (P7.10 / `mtui-rs-odq8`), which calls it when it evicts
    /// a session (idle-TTL sweep or explicit eviction). Mirrors the REPL `quit`
    /// disconnect path — [`HostsGroup::close`](mtui_hosts::HostsGroup::close) per
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
    /// [`DISCONNECT_TIMEOUT`]: a wedged host close is logged and abandoned so
    /// `close()` — and the registry idle-sweep awaiting it — always returns.
    ///
    /// ## Rust deviation
    ///
    /// Upstream clears `report.targets` after closing; the Rust `HostsGroup::close`
    /// (like the REPL `quit`) closes each `Target` but leaves it in the group with
    /// its now-dead connection — the report and its host group are dropped whole
    /// when the session is evicted. So this does not empty the groups; a closed
    /// target simply reports its connection inactive/closed.
    pub async fn close(&self) {
        self.close_with_timeout(DISCONNECT_TIMEOUT).await;
    }

    /// [`close`](Self::close) with an explicit fan-out budget.
    ///
    /// The timeout seam upstream exposes as `_disconnect_targets(timeout=...)`,
    /// kept `pub(crate)` so the wedged-close unit test can bound the wait to a
    /// fraction of a second instead of 45s.
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
    /// The central MCP dispatch primitive (the Rust analogue of upstream
    /// `McpSession.run_command`): it dispatches `name`/`argv` through the **same**
    /// engine the REPL uses (a forked-session [`dispatch_command`] on the
    /// concurrent path, [`dispatch_argv`] on the canonical session for the
    /// exclusive path), then returns what the command wrote to the call's own
    /// captured display — passed through [`cap_output`] so one large result cannot
    /// dwarf the client's context.
    ///
    /// Before dispatch the call takes its [`command_lock`](Self::command_lock):
    /// a single-template call holds the registry gate *shared* plus its per-RRID
    /// lock (so same-RRID calls serialise, different-RRID calls take distinct
    /// locks), while fan-out / mutators take the gate *exclusive*. A single-RRID
    /// (non-mutator) call then dispatches on a
    /// [`Session::fork_for_call`](mtui_core::Session::fork_for_call) — sharing the
    /// report entry locks, with its own display — spawned so it runs in genuine
    /// parallel with a concurrent different-RRID call (`mtui-rs-f36r`); the
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
        // Acquire the per-template / registry-gate hold for this invocation
        // *before* touching the session, so same-RRID and unscoped calls
        // serialise and mutators drain in-flight per-RRID work. Held for the
        // whole dispatch, released when `_lock` drops at end of scope.
        let lock = self.command_lock(registry, name, argv).await;

        // Per-call output isolation (bead `mtui-rs-f36r`, step 3): give this
        // dispatch its *own* fresh capture buffer + display so two overlapping
        // calls never write into the same buffer and clobber each other's stdout.
        // Bounded to the same budget as the session-wide sink.
        let call_buf = SharedBuf::with_limit(self.max_output_bytes);
        let call_display =
            CommandPromptDisplay::with_sink(Box::new(call_buf.clone()), ColorMode::Never);

        let result = match &lock {
            // Concurrent path (bead `mtui-rs-f36r`, steps 4-5): a single-real-RRID
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
                let result = dispatch_argv(registry, &mut session, name, argv).await;
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
    /// The Rust analogue of upstream `run_command(..., ctx=..., progress_interval)`:
    /// when `sink` is `Some`, the whole dispatch (including the lock wait, exactly
    /// as upstream wraps inside `_command_lock`) is raced against a heartbeat that
    /// fires every `interval` via [`run_with_heartbeat`], so a slow foreground call
    /// does not time the client out. A `None` sink takes the original zero-overhead
    /// path — [`run_command`](Self::run_command) verbatim (upstream `ctx is None`).
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
    /// The Rust analogue of upstream `_resolve_job_rrids`: it resolves `argv`
    /// exactly as the foreground dispatch does (via [`resolve_command_rrids`],
    /// which parses the command's own clap parser and applies its
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
    /// rejected as a whole (no partial spawn — bead `mtui-rs-th4o.8`). Must be
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
    /// The Rust analogue of upstream `_mint_job`. The worker runs through
    /// [`run_command`](Self::run_command) (so it takes the same per-RRID /
    /// registry gate and output cap as a foreground call) and records the
    /// terminal state/result on the job's `Arc<StdMutex<Job>>`; on settling it
    /// FIFO-evicts terminal records past the completed cap. `self` is an `Arc`
    /// because the spawned task must own the session for its `'static` lifetime.
    /// The caller holds the jobs lock so the admit-check and the insert are
    /// atomic against a concurrent spawn.
    fn mint_job(
        self: &Arc<Self>,
        jobs_guard: &mut HashMap<String, Arc<StdMutex<Job>>>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
        job_id: String,
    ) -> String {
        let job = Arc::new(StdMutex::new(Job {
            id: job_id.clone(),
            command: name.to_owned(),
            state: JobState::Running,
            started: Instant::now(),
            finished: None,
            result: None,
            error: None,
            exit_code: None,
            handle: None,
        }));
        jobs_guard.insert(job_id.clone(), Arc::clone(&job));

        let session = Arc::clone(self);
        let name = name.to_owned();
        let worker_job = Arc::clone(&job);
        let handle = tokio::spawn(async move {
            let outcome = session.run_command(&registry, &name, &argv).await;
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
    /// The Rust analogue of upstream `start_job`: mints exactly **one** job
    /// (id `"<command>-<n>"`) and returns immediately with a handle, so the
    /// client is not held for the minutes a `run`/`update`/`downgrade` can take.
    /// The tool layer calls [`start_jobs`](Self::start_jobs) instead so a
    /// fanned-out slow command yields one job per template; this stays the
    /// single-job primitive for tests and non-fan-out callers.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) when the session is already at
    /// [`max_active_jobs`](Self::max_active_jobs) running jobs; no worker is
    /// spawned in that case.
    pub fn start_job(
        self: &Arc<Self>,
        registry: Arc<Registry>,
        name: &str,
        argv: Vec<String>,
    ) -> Result<String, McpCommandError> {
        let mut jobs = self.jobs.lock().expect("jobs table poisoned");
        self.admit(&jobs, 1)?;
        let n = self.job_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let job_id = format!("{name}-{n}");
        Ok(self.mint_job(&mut jobs, registry, name, argv, job_id))
    }

    /// Start `name`/`argv` in the background, fanning out one job per template.
    ///
    /// The Rust analogue of upstream `start_jobs`: resolves the target templates
    /// exactly as the foreground path does (via
    /// [`resolve_job_rrids`](Self::resolve_job_rrids)). When more than one
    /// template resolves, mints **one job per template** — each running `argv`
    /// scoped to that template with `-T <rrid>` **prepended** (a positional
    /// `REMAINDER` command like `run` would otherwise swallow a trailing
    /// `-T <rrid>` into its own value) — so a backgrounded fanned-out slow
    /// command is independently observable and cancellable per template. When a
    /// single template (or none) resolves, this is exactly one job with the
    /// unchanged `<command>-<n>` id.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) when spawning the resolved jobs would breach
    /// [`max_active_jobs`](Self::max_active_jobs); the whole fan-out is rejected
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
                        let mut scoped_argv = vec!["-T".to_owned(), rrid];
                        scoped_argv.extend(argv.iter().cloned());
                        self.mint_job(&mut jobs, Arc::clone(&registry), name, scoped_argv, job_id)
                    })
                    .collect())
            }
            // Single template, none, or a client-supplied `-T` already narrowing
            // to one: keep the single-job path (and its stable id shape).
            _ => Ok(vec![self.start_job(registry, name, argv)?]),
        }
    }

    /// A poll-facing snapshot of one job record (upstream `_job_view`).
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

    /// Return a view of every job started in this session (upstream `job_list`).
    #[must_use]
    pub fn job_list(&self) -> Vec<JobView> {
        self.jobs
            .lock()
            .expect("jobs table poisoned")
            .values()
            .map(|j| Self::view(&j.lock().expect("job record poisoned")))
            .collect()
    }

    /// Return `job_id`'s state view, or an error if unknown (upstream
    /// `job_status`).
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) with `"no such job: <id>"` when `job_id` is
    /// not in the table.
    pub fn job_status(&self, job_id: &str) -> Result<JobView, McpCommandError> {
        let job = self.job(job_id)?;
        Ok(Self::view(&job.lock().expect("job record poisoned")))
    }

    /// Return a finished job's stdout, or the right failure envelope (upstream
    /// `job_result`).
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
            JobState::Cancelled => Err(McpCommandError {
                stdout: String::new(),
                stderr: format!("job {job_id} was cancelled"),
                exit_code: 1,
            }),
            JobState::Done => Ok(job.result.clone().unwrap_or_default()),
        }
    }

    /// Cancel a running job; error if the id is unknown (upstream `job_cancel`).
    ///
    /// Aborts the worker task and marks the record `Cancelled`. NOTE: if the job
    /// is mid host-op (an SSH/subprocess body), aborting detaches the awaiter but
    /// the underlying host operation may keep running to completion — the same
    /// caveat as interrupting a foreground `run`. A finished job is a no-op.
    ///
    /// # Errors
    ///
    /// [`McpCommandError`] (exit 1) with `"no such job: <id>"` when unknown.
    pub async fn job_cancel(&self, job_id: &str) -> Result<String, McpCommandError> {
        let job = self.job(job_id)?;
        let handle = {
            let mut j = job.lock().expect("job record poisoned");
            if j.state == JobState::Running {
                j.state = JobState::Cancelled;
                j.finished = Some(Instant::now());
                j.handle.take()
            } else {
                None
            }
        };
        if let Some(handle) = handle {
            handle.abort();
            // Await the aborted task so cancellation has fully unwound before we
            // return; a `JoinError::Cancelled` is expected and ignored.
            let _ = handle.await;
            // The aborted worker's terminal-write branch (which normally evicts)
            // was skipped, so reap history here now this record is terminal.
            self.evict_completed();
        }
        Ok(format!("cancelled job {job_id}"))
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
    use super::*;
    use mtui_core::register_all;

    fn session(config: Config) -> Arc<McpSession> {
        McpSession::new(config)
    }

    /// Each session gets a distinct id, and a session's id is stable across
    /// calls — the freshness invariant `remint_after_drop_is_a_new_session`
    /// relies on instead of `Arc` address identity (bead `mtui-rs-1edj`).
    #[test]
    fn session_id_is_unique_and_stable() {
        let a = McpSession::new(Config::default());
        let b = McpSession::new(Config::default());
        assert_ne!(a.id(), b.id(), "distinct sessions must have distinct ids");
        assert_eq!(a.id(), a.id(), "a session's id is stable across calls");
    }

    /// A host whose `close()` never returns must not block `close_with_timeout`.
    ///
    /// The Rust analogue of upstream
    /// `test_disconnect_targets_bounded_wait_survives_a_wedged_close`: with a
    /// small budget, teardown returns despite the stuck close, the healthy host
    /// is still closed, and the abandoned close is later released so its task
    /// unwinds. Bounding via [`tokio::time::timeout`] (not a thread-pool
    /// `shutdown(wait=False)`) is the whole point — see the module docs.
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
    /// wired (upstream `prompter = None`; `interactive = false` is provided by
    /// `capture::session` passing `is_repl = false`).
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
                state,
                started: base,
                finished,
                result: None,
                error: None,
                exit_code: None,
                handle: None,
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
                        state: JobState::Done,
                        started: Instant::now(),
                        finished: Some(Instant::now()),
                        result: None,
                        error: None,
                        exit_code: None,
                        handle: None,
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
    /// exclusively (upstream unscoped fallback).
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
    /// added in `mtui-rs-f36r` steps 4-5: a structural mutation must land on the
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
        assert_eq!(msg, format!("cancelled job {job_id}"));
        // State is unchanged: a finished job is not rewritten to Cancelled.
        assert_eq!(sess.job_status(&job_id).unwrap().state, JobState::Done);
    }

    /// `job_result` on a cancelled job surfaces the "was cancelled" envelope.
    #[tokio::test]
    async fn job_result_cancelled_job_raises() {
        let sess = session(Config::default());
        // Seed a cancelled record directly (no worker needed for this read path).
        let job = Arc::new(StdMutex::new(Job {
            id: "whoami-1".to_owned(),
            command: "whoami".to_owned(),
            state: JobState::Cancelled,
            started: Instant::now(),
            finished: Some(Instant::now()),
            result: None,
            error: None,
            exit_code: None,
            handle: None,
        }));
        sess.jobs.lock().unwrap().insert("whoami-1".to_owned(), job);

        let err = sess
            .job_result("whoami-1")
            .expect_err("cancelled job raises on job_result");
        assert!(err.stderr.contains("was cancelled"), "got: {err:?}");
        assert_eq!(err.exit_code, 1);
    }

    // ---- progress heartbeats (bead mtui-rs-76e.14) ------------------------ //

    /// Records every frame `report` receives; the Rust analogue of upstream's
    /// `_RecordingCtx`.
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
    /// working sink. The Rust analogue of upstream `_FailingCtx`: it lets us assert
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
    /// bare `run_command` (upstream `test_ctx_none_emits_no_progress...`).
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
    /// carrying the command name; the future's output is returned unchanged
    /// (upstream `test_heartbeat_fires...` + `..._monotonic`). Driven directly
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

    /// A future that finishes well inside the interval fires zero frames
    /// (upstream `test_no_heartbeat_for_fast_command`).
    #[tokio::test]
    async fn run_with_heartbeat_no_frames_for_fast_future() {
        let sink = RecordingSink::default();
        let out = run_with_heartbeat(async { 7 }, &sink, "fast", Duration::from_secs(1)).await;
        assert_eq!(out, 7);
        assert!(sink.calls().is_empty(), "no frames: {:?}", sink.calls());
    }

    /// A failing command surfaces `McpCommandError` unchanged through the
    /// heartbeat path (upstream `test_command_exception_propagates...`).
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
    /// future still returns its value and the sink's attempts are recorded
    /// (upstream `test_progress_send_failure_is_swallowed`).
    #[tokio::test]
    async fn run_with_heartbeat_send_failure_does_not_mask_result() {
        let sink = FailingSink::default();
        let body = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            "ok"
        };

        let out =
            run_with_heartbeat(body, &sink, "_sleepy_command", Duration::from_millis(40)).await;
        assert_eq!(out, "ok", "result survives a failing sink");
        assert!(
            *sink.calls.lock().unwrap() >= 1,
            "at least one heartbeat was attempted"
        );
    }
}
