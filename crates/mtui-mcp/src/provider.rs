//! The http [`SessionRegistry`] factory with its cap + idle-TTL enforcement.
//!
//! Under stdio one process serves one client with a single [`McpSession`]. Under
//! http the [`SessionRegistry`] *factory* (`try_make_server`), which rmcp's
//! streamable-HTTP transport calls once per new MCP session, binds a fresh
//! isolated session per client; rmcp owns the `Mcp-Session-Id` keying and the
//! transport teardown.
//!
//! ## Cap + idle-TTL
//!
//! rmcp has no max-sessions or idle-TTL knob, its `service_factory` receives
//! **no** session key, and it offers no teardown hook. So the `[mcp]
//! session_cap` / `session_idle_timeout` bounds are enforced
//! **application-side, wrapped around the factory**:
//!
//! * the **cap**: [`SessionRegistry::try_make_server`] refuses a new session past
//!   `session_cap` with an [`io::Error`] (surfaced as an internal-error HTTP
//!   response), bounding the fleet of SSH-holding sessions;
//! * the idle **sweeper** ([`SessionRegistry::spawn_sweeper`]): evicts +
//!   [`McpSession::close`]-es any session untouched for `session_idle_timeout`
//!   seconds (reclaiming its SSH host connections); `0` disables it.
//!
//! Each minted [`McpServer`] carries a [`SessionGuard`] that removes the session
//! from the live set on `Drop` (so a session rmcp tears down frees a cap slot
//! with no teardown hook), spawns that session's [`McpSession::close`], and
//! shares the last-touch timestamp [`McpServer`] bumps per tool call. Activity is therefore measured at the *tool-call* boundary;
//! pure SSE-GET keepalive traffic (owned by rmcp, invisible here) is not counted
//! — acceptable for a DoS/idle guard.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use mtui_config::Config;
use mtui_core::{HOST_CLOSE_TIMEOUT, LogLevel, Registry};
use mtui_hosts::OpLockHolders;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::server::McpServer;
use crate::session::McpSession;

/// A monotonic clock reading in milliseconds, for last-touch bookkeeping.
///
/// Measured against a process-lifetime epoch so the value is a plain comparable
/// `u64` shareable through an [`AtomicU64`] (an `Instant` is not atomically
/// storable). Only *differences* are meaningful.
pub(crate) fn mono_millis() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_millis() as u64
}

/// The process's runtime log-level control. The `tracing` filter is
/// process-wide, so every session the server mints shares this one handle.
pub type LogLevelControl = Arc<dyn Fn(LogLevel) + Send + Sync>;

/// Point `session`'s `set_log_level` at the process-wide `control`.
pub(crate) fn install_log_level(session: &McpSession, control: &LogLevelControl) {
    let control = Arc::clone(control);
    session
        .session()
        .try_lock()
        .expect("a session minted a moment ago is uncontended")
        .set_log_level_sink(Box::new(move |level| control(level)));
}

/// One tracked live session in the [`SessionRegistry`]'s live set.
///
/// Only a [`Weak`] is held (rmcp owns the strong [`McpServer`], and the
/// `Arc<McpSession>` inside it), so a session rmcp already dropped upgrades to
/// `None` and is pruned. `last_touch` is shared with that [`McpServer`].
struct TrackedSession {
    session: Weak<McpSession>,
    last_touch: Arc<AtomicU64>,
}

/// The registry's shared live-session table: sessions keyed by a monotonic id.
#[derive(Default)]
struct LiveState {
    sessions: HashMap<u64, TrackedSession>,
    /// Closes spawned by a dropped [`SessionGuard`] or a sweep, awaited by
    /// `close_all` so neither a cancelled sweep nor shutdown abandons them.
    closing: Vec<JoinHandle<()>>,
}

impl LiveState {
    fn insert(&mut self, id: u64, session: Weak<McpSession>, last_touch: Arc<AtomicU64>) {
        self.sessions.insert(
            id,
            TrackedSession {
                session,
                last_touch,
            },
        );
    }

    fn remove(&mut self, id: u64) {
        self.sessions.remove(&id);
    }

    fn clear(&mut self) {
        self.sessions.clear();
    }

    /// Sessions whose server is already gone.
    fn prune_dead(&mut self) {
        let dead: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, t)| t.session.strong_count() == 0)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.remove(id);
        }
    }

    /// Strong handles to every live session, skipping the dead.
    fn upgrades(&self) -> Vec<Arc<McpSession>> {
        self.sessions
            .values()
            .filter_map(|t| t.session.upgrade())
            .collect()
    }

    /// Live sessions only; a server rmcp already dropped is pruned lazily and
    /// is not counted here in the meantime.
    fn live_len(&self) -> usize {
        self.sessions
            .values()
            .filter(|t| t.session.strong_count() > 0)
            .count()
    }
}

/// The registry's shared live-session table, behind one mutex.
type LiveSet = Arc<StdMutex<LiveState>>;

/// RAII handle owned by each minted [`McpServer`]: unregisters its session on
/// `Drop` and closes it.
///
/// When rmcp drops the `McpServer` at session close, this frees a
/// `session_cap` slot and spawns the session's [`McpSession::close`]. The close
/// runs even if the sweeper or `SessionRegistry::close_all` already closed the
/// session: closing is idempotent, and a session rmcp kept serving after a
/// sweep may have attached hosts since.
pub struct SessionGuard {
    live: LiveSet,
    id: u64,
    /// Strong, because the server's own `Arc` drops before this guard does.
    session: Arc<McpSession>,
    /// The runtime that minted the session, so a drop on any thread can close it.
    runtime: Option<tokio::runtime::Handle>,
    /// Bounds concurrent guard closes at `[mcp] sweep_parallel`.
    permits: Arc<tokio::sync::Semaphore>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.live.lock() {
            state.remove(self.id);
        }
        let Some(runtime) = &self.runtime else {
            tracing::debug!(id = self.id, "session minted outside a runtime; not closed");
            return;
        };
        let session = Arc::clone(&self.session);
        let permits = Arc::clone(&self.permits);
        spawn_tracked(&self.live, runtime, async move {
            let _permit = permits.acquire_owned().await;
            session.close().await;
        });
    }
}

/// Spawns `close` on `runtime` and records it for `close_all` to await.
///
/// `Handle::spawn` on a runtime that is shutting down drops the task instead
/// of panicking, so this is safe from any `Drop`.
fn spawn_tracked(
    live: &LiveSet,
    runtime: &tokio::runtime::Handle,
    close: impl Future<Output = ()> + Send + 'static,
) {
    let task = runtime.spawn(close);
    if let Ok(mut state) = live.lock() {
        state.closing.retain(|task| !task.is_finished());
        state.closing.push(task);
    }
}

/// The http per-client session factory with cap + idle-TTL enforcement.
///
/// Each client must see **only its own** loaded template + SSH `targets`, so
/// [`try_make_server`](Self::try_make_server), invoked once per session by rmcp,
/// mints a **fresh, fully isolated** [`McpServer`] with its own [`McpSession`].
///
/// The live set holds [`Weak`] handles (rmcp owns the session map) and carries
/// the cap + idle-TTL bounds described in the module docs.
#[derive(Clone)]
pub struct SessionRegistry {
    /// The shared command registry every minted server dispatches against.
    registry: Arc<Registry>,
    /// Cloned per session, isolating any scalar a command rebinds on `config`.
    config: Config,
    /// Ceiling on concurrent live sessions (`[mcp] session_cap`).
    cap: usize,
    /// Idle-TTL before a quiet session is swept (`[mcp] session_idle_timeout`);
    /// `Duration::ZERO` disables sweeping.
    idle_timeout: Duration,
    /// Max stale sessions torn down concurrently per sweep (`[mcp]
    /// sweep_parallel`); always `>= 1` (validated in config).
    sweep_parallel: usize,
    /// The live-session table, shared with every [`SessionGuard`] + the sweeper.
    live: LiveSet,
    /// `sweep_parallel` permits shared by every [`SessionGuard`]'s close.
    close_permits: Arc<tokio::sync::Semaphore>,
    /// Monotonic session-id counter (each mint gets a fresh id).
    next_id: Arc<AtomicU64>,
    /// Shared by every minted session: they all live in one process, so their
    /// targets' operation locks all read as ours on the wire and only this
    /// registry tells one session's hold from another's.
    op_lock_holders: OpLockHolders,
    /// Installed into every minted session, so all of them drive the one
    /// process filter.
    log_level: Option<LogLevelControl>,
}

impl SessionRegistry {
    /// Builds the factory from the shared command `registry` and a base `config`
    /// (which carries the cap, idle-TTL and sweep fan-out bound).
    #[must_use]
    pub fn new(registry: Arc<Registry>, config: Config) -> Self {
        let cap = config.mcp_session_cap;
        let idle_timeout = Duration::from_secs(config.mcp_session_idle_timeout);
        let sweep_parallel = config.mcp_sweep_parallel.max(1);
        Self {
            registry,
            config,
            cap,
            idle_timeout,
            sweep_parallel,
            live: Arc::new(StdMutex::new(LiveState::default())),
            close_permits: Arc::new(tokio::sync::Semaphore::new(sweep_parallel)),
            next_id: Arc::new(AtomicU64::new(0)),
            op_lock_holders: OpLockHolders::default(),
            log_level: None,
        }
    }

    /// Lets `set_log_level` in every session minted from now on change the
    /// process log level through `control`.
    #[must_use]
    pub fn with_log_level(mut self, control: LogLevelControl) -> Self {
        self.log_level = Some(control);
        self
    }

    /// The configured concurrent-session cap.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The configured idle-TTL (`Duration::ZERO` disables sweeping).
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// The number of live sessions currently tracked. A server rmcp already
    /// dropped is pruned lazily (by [`try_make_server`](Self::try_make_server) or
    /// the sweeper) and is not counted here in the meantime.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live.lock().map(|s| s.live_len()).unwrap_or(0)
    }

    /// Strong handles to every live session, for inspection: upgrades each
    /// tracked [`Weak`], skipping the dead. Lets callers (and the sweeper tests)
    /// observe a session's teardown after an eviction.
    #[must_use]
    pub fn live_sessions(&self) -> Vec<Arc<McpSession>> {
        self.live.lock().map(|s| s.upgrades()).unwrap_or_default()
    }

    /// Mint a fresh [`McpSession`], cloning the base [`Config`] so its mutable
    /// scalar state is independent (own `metadata` / `targets` / capture sink).
    /// The isolation boundary: [`try_make_server`](Self::try_make_server) wraps
    /// it for the transport, and tests use it directly. Sessions minted here
    /// serve HTTP, so their audit `transport` agrees with the server's.
    #[must_use]
    pub fn make_session(&self) -> Arc<McpSession> {
        let session = McpSession::new_with_transport(self.config.clone(), "http");
        session
            .session()
            .try_lock()
            .expect("a session minted a moment ago is uncontended")
            .set_op_lock_holders(self.op_lock_holders.clone());
        if let Some(control) = &self.log_level {
            install_log_level(&session, control);
        }
        session
    }

    /// Mint a fresh, isolated, cap-checked [`McpServer`] for one MCP session.
    ///
    /// On success it registers the session's [`Weak`] handle + a shared
    /// last-touch timestamp, and hands the [`McpServer`] a [`SessionGuard`]
    /// (frees the slot on `Drop`) plus that last-touch handle.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] of kind [`io::ErrorKind::Other`] when the live
    /// set already holds [`cap`](Self::cap) sessions.
    pub fn try_make_server(&self) -> io::Result<McpServer> {
        let mut state = self
            .live
            .lock()
            .map_err(|_| io::Error::other("session registry lock poisoned"))?;

        // Drop dead entries so a burst of disconnects+reconnects cannot
        // spuriously trip the cap.
        state.prune_dead();

        // Pruned above, so every tracked session is live: count directly
        // instead of re-filtering.
        if state.sessions.len() >= self.cap {
            return Err(io::Error::other(format!(
                "session registry full: {} concurrent client sessions already \
                 active; retry once a session is released or raise \
                 [mcp] session_cap",
                self.cap
            )));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session = self.make_session();
        let last_touch = Arc::new(AtomicU64::new(mono_millis()));
        state.insert(id, Arc::downgrade(&session), Arc::clone(&last_touch));
        drop(state);

        let guard = SessionGuard {
            live: Arc::clone(&self.live),
            id,
            session: Arc::clone(&session),
            runtime: tokio::runtime::Handle::try_current().ok(),
            permits: Arc::clone(&self.close_permits),
        };
        Ok(McpServer::new_tracked(
            Arc::clone(&self.registry),
            session,
            guard,
            last_touch,
        ))
    }

    /// Start the background idle-TTL sweeper.
    ///
    /// Wakes every `max(1s, idle_timeout / 2)`, collects live sessions untouched
    /// for at least the timeout, re-validates each for staleness under the lock
    /// immediately before removing it (so a session handed back to a client
    /// mid-sweep is spared), then [`McpSession::close`]s the confirmed-stale set
    /// (best-effort, idempotent) to reclaim its SSH host connections. The closes
    /// run **concurrently under `sweep_parallel`** with a single per-cycle
    /// deadline, so one wedged host teardown cannot serialize the rest.
    ///
    /// Returns `None` (spawning nothing) when the idle-TTL is zero; otherwise the
    /// [`JoinHandle`] lets the caller await the task after cancelling.
    pub fn spawn_sweeper(&self, cancel: CancellationToken) -> Option<JoinHandle<()>> {
        if self.idle_timeout.is_zero() {
            return None;
        }
        let live = Arc::clone(&self.live);
        let timeout = self.idle_timeout;
        let parallel = self.sweep_parallel;
        Some(tokio::spawn(async move {
            sweep_loop(live, timeout, parallel, cancel).await;
        }))
    }

    /// Tear down **every** live session, for graceful process shutdown.
    ///
    /// The HTTP counterpart to the stdio serve loop's [`McpSession::close`].
    /// Cancelling the sweeper alone does **not** release live sessions (the
    /// registry holds only [`Weak`] handles whose `Drop` cannot run the async
    /// pool-claim release), so a clean shutdown would otherwise leak every
    /// active session's pool claims and SSH connections.
    ///
    /// Closes concurrently under [`sweep_parallel`](Self::sweep_parallel) with a
    /// single overall deadline so one wedged host close cannot hang process exit.
    /// Best-effort and idempotent: a second call finds an empty set.
    pub(crate) async fn close_all(&self) {
        let deadline = tokio::time::Instant::now() + HOST_CLOSE_TIMEOUT + Duration::from_secs(1);
        // Repeated until both are empty: a guard may drop while we wait.
        loop {
            let (sessions, closing) = {
                let mut state = match self.live.lock() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let sessions = state.upgrades();
                state.clear();
                (sessions, std::mem::take(&mut state.closing))
            };
            if sessions.is_empty() && closing.is_empty() {
                return;
            }
            let round = async {
                tokio::join!(
                    close_sessions(sessions, self.sweep_parallel),
                    futures::future::join_all(closing)
                )
            };
            if tokio::time::timeout_at(deadline, round).await.is_err() {
                tracing::warn!("session teardown timed out; abandoning remaining closes");
                return;
            }
        }
    }
}

/// Close a batch of sessions concurrently under `parallel`, bounded by a single
/// overall deadline (`HOST_CLOSE_TIMEOUT + 1s`).
///
/// Shared by the idle sweep ([`sweep_once`]) and graceful shutdown
/// (`SessionRegistry::close_all`). One budget, not N×, so the batch returns
/// even if every close wedges and teardown latency stays ~independent of session
/// count.
async fn close_sessions(sessions: Vec<Arc<McpSession>>, parallel: usize) {
    use futures::stream::StreamExt as _;

    if sessions.is_empty() {
        return;
    }
    let batch =
        futures::stream::iter(sessions).for_each_concurrent(parallel, |session| async move {
            session.close().await;
        });
    let deadline = HOST_CLOSE_TIMEOUT + Duration::from_secs(1);
    if tokio::time::timeout(deadline, batch).await.is_err() {
        tracing::warn!(
            ?deadline,
            "session teardown timed out; abandoning remaining closes"
        );
    }
}

/// Collect the ids + strong handles of sessions idle for at least `timeout`.
///
/// Snapshots under the live-set lock (no `await` held), pruning dead entries as
/// a side effect. Returns `(id, session)` tuples so the caller can re-validate +
/// close without re-locking per entry.
fn collect_stale(live: &LiveSet, timeout: Duration, now: u64) -> Vec<(u64, Arc<McpSession>)> {
    let mut state = match live.lock() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    state.prune_dead();
    let timeout_ms = timeout.as_millis() as u64;
    state
        .sessions
        .iter()
        .filter(|(_, t)| now.saturating_sub(t.last_touch.load(Ordering::Relaxed)) >= timeout_ms)
        .filter_map(|(id, t)| t.session.upgrade().map(|s| (*id, s)))
        .collect()
}

/// The idle-sweeper body: periodically evict + close quiet sessions.
async fn sweep_loop(live: LiveSet, timeout: Duration, parallel: usize, cancel: CancellationToken) {
    let interval = Duration::from_millis((timeout.as_millis() as u64 / 2).max(1000));
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        // Under the same `select!` as the wake, so a cancel preempts a slow
        // teardown batch instead of waiting it out.
        tokio::select! {
            () = cancel.cancelled() => return,
            () = sweep_once(&live, timeout, parallel) => {}
        }
    }
}

/// One sweep cycle: confirm the stale set under the lock, then close it
/// concurrently under `parallel` with a single overall deadline.
async fn sweep_once(live: &LiveSet, timeout: Duration, parallel: usize) {
    let now = mono_millis();
    let timeout_ms = timeout.as_millis() as u64;

    // Phase 1 (serial, lock-held): the re-read and the removal run with no await
    // between them, so a client refresh cannot slip into that gap. Only sessions
    // removed here are closed, so a spared session is never torn down.
    let mut to_close: Vec<Arc<McpSession>> = Vec::new();
    for (id, session) in collect_stale(live, timeout, now) {
        let mut state = match live.lock() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(tracked) = state.sessions.get(&id) else {
            continue; // already evicted elsewhere
        };
        let touched = tracked.last_touch.load(Ordering::Relaxed);
        if mono_millis().saturating_sub(touched) < timeout_ms {
            tracing::info!(id, "skipping sweep: session re-activated");
            continue;
        }
        state.remove(id);
        drop(state);
        tracing::info!(id, "sweeping idle MCP session");
        to_close.push(session);
    }
    if to_close.is_empty() {
        return;
    }

    // Phase 2 (concurrent, unlocked), as a tracked task: entries are already
    // out of the live set, so a sweep cancelled at shutdown must leave its
    // closes for `close_all` to await rather than abandon them.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    spawn_tracked(live, &tokio::runtime::Handle::current(), async move {
        close_sessions(to_close, parallel).await;
        let _ = done_tx.send(());
    });
    let _ = done_rx.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_reads_bounds_from_config() {
        let mut config = Config::default();
        config.mcp_session_cap = 4;
        config.mcp_session_idle_timeout = 120;
        config.mcp_sweep_parallel = 3;
        let reg = SessionRegistry::new(Arc::new(mtui_core::register_all()), config);
        assert_eq!(reg.cap(), 4);
        assert_eq!(reg.idle_timeout(), Duration::from_secs(120));
        assert_eq!(reg.sweep_parallel, 3);
        assert_eq!(reg.live_count(), 0);
    }

    /// Build a registry with the given idle-TTL over a default config.
    fn reg_with_idle(idle: Duration) -> SessionRegistry {
        let mut config = Config::default();
        config.mcp_session_cap = 32;
        config.mcp_session_idle_timeout = idle.as_secs();
        let mut reg = SessionRegistry::new(Arc::new(mtui_core::register_all()), config);
        // Allow sub-second TTLs in tests regardless of the whole-second config key.
        reg.idle_timeout = idle;
        reg
    }

    /// Register a session directly in the live set (bypassing the cap-checked
    /// factory) with a controllable last-touch. The strong `Arc` is the
    /// caller's.
    fn track(
        reg: &SessionRegistry,
        session: &Arc<McpSession>,
        touch: u64,
    ) -> (u64, Arc<AtomicU64>) {
        let id = reg.next_id.fetch_add(1, Ordering::Relaxed);
        let last_touch = Arc::new(AtomicU64::new(touch));
        reg.live
            .lock()
            .unwrap()
            .insert(id, Arc::downgrade(session), Arc::clone(&last_touch));
        (id, last_touch)
    }

    #[tokio::test]
    async fn sweeper_evicts_stale_session() {
        let reg = reg_with_idle(Duration::from_millis(200));
        let session = reg.make_session();
        // The sweeper's first cycle (>= 1s later) sees it aged past the 200ms TTL.
        let (_id, _touch) = track(&reg, &session, mono_millis());
        assert_eq!(reg.live_count(), 1);

        let cancel = CancellationToken::new();
        let sweeper = reg.spawn_sweeper(cancel.clone()).unwrap();

        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if reg.live_count() == 0 {
                break;
            }
        }
        cancel.cancel();
        let _ = sweeper.await;
        assert_eq!(reg.live_count(), 0, "stale session must be swept");
    }

    /// The idle sweep must reclaim a host attached with **nothing loaded** — a
    /// case `live_count` alone (this file's other oracle) can never see, because
    /// the session is evicted either way while the host stays connected+locked.
    #[tokio::test]
    async fn sweeper_evicts_null_group_hosts() {
        use mtui_hosts::{MockConnection, TARGET_LOCK_PATH, Target};
        use mtui_types::enums::TargetState;

        let reg = reg_with_idle(Duration::from_millis(200));
        let probe = MockConnection::new("n1");
        let mut target =
            Target::with_connection("n1", TargetState::Enabled, Box::new(probe.clone()));
        target.lock("").await.expect("operation lock taken");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_some(),
            "fixture must arm the assertion — the remote lock exists before the sweep"
        );

        let session = reg.make_session();
        {
            let mut guard = session.session().lock().await;
            assert!(
                !guard.metadata().is_loaded(),
                "fixture must reach the no-template state"
            );
            guard.targets_mut().add(target);
        }
        track(&reg, &session, mono_millis());

        let cancel = CancellationToken::new();
        let sweeper = reg.spawn_sweeper(cancel.clone()).unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if reg.live_count() == 0 {
                break;
            }
        }
        cancel.cancel();
        let _ = sweeper.await;

        assert_eq!(reg.live_count(), 0, "stale session must be swept");
        assert!(probe.is_closed(), "n1: disconnected by the sweep");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_none(),
            "n1: remote operation lock released by the sweep"
        );
    }

    #[tokio::test]
    async fn sweeper_spares_freshly_touched_session() {
        let reg = reg_with_idle(Duration::from_millis(200));
        let session = reg.make_session();
        let (_id, touch) = track(&reg, &session, mono_millis());

        let cancel = CancellationToken::new();
        let sweeper = reg.spawn_sweeper(cancel.clone()).unwrap();

        // Keep touching under the TTL for ~600ms (several sweep cycles).
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            touch.store(mono_millis(), Ordering::Relaxed);
        }
        let alive = reg.live_count();
        cancel.cancel();
        let _ = sweeper.await;
        assert_eq!(alive, 1, "a freshly-touched session must not be swept");
    }

    /// Re-activated after the stale snapshot but before eviction: the pre-close
    /// re-check must spare it.
    #[tokio::test]
    async fn sweeper_respects_reactivation_before_close() {
        let reg = reg_with_idle(Duration::from_millis(200));
        let session = reg.make_session();
        // Age the entry into staleness by elapsing the TTL, rather than
        // back-dating: robust regardless of the monotonic epoch's value.
        let (id, touch) = track(&reg, &session, mono_millis());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let now = mono_millis();
        let stale = collect_stale(&reg.live, reg.idle_timeout, now);
        assert_eq!(stale.len(), 1, "aged session is stale");

        // A client re-activates it before the sweep evicts.
        touch.store(mono_millis(), Ordering::Relaxed);

        // Drive the loop's per-entry re-check manually.
        let timeout_ms = reg.idle_timeout.as_millis() as u64;
        {
            let state = reg.live.lock().unwrap();
            let tracked = state.sessions.get(&id).unwrap();
            let touched = tracked.last_touch.load(Ordering::Relaxed);
            assert!(
                mono_millis().saturating_sub(touched) < timeout_ms,
                "re-check must see the fresh touch and spare the session"
            );
        }
        assert_eq!(reg.live_count(), 1, "re-activated session still tracked");
    }

    /// Build a session whose single host's `close()` blocks until `gate` fires.
    async fn wedged_session(gate: Arc<tokio::sync::Notify>) -> Arc<McpSession> {
        use mtui_hosts::{HostsGroup, MockConnection, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let conn = MockConnection::new("slow-host").with_blocking_close(gate);
        let target = Target::with_connection("slow-host", TargetState::Enabled, Box::new(conn));
        let sess = McpSession::new(Config::default());
        {
            let mut guard = sess.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
            report.base_mut().targets = HostsGroup::new(vec![target], false);
            guard.templates.add(Box::new(report));
            guard.templates.set_active("SUSE:Maintenance:1:1");
        }
        sess
    }

    /// A session whose single host's `close()` takes ~`delay` and then returns,
    /// for timing the sweep fan-out.
    async fn slow_close_session(delay: Duration) -> Arc<McpSession> {
        use mtui_hosts::{HostsGroup, MockConnection, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let conn = MockConnection::new("slow-host").with_close_delay(delay);
        let target = Target::with_connection("slow-host", TargetState::Enabled, Box::new(conn));
        let sess = McpSession::new(Config::default());
        {
            let mut guard = sess.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
            report.base_mut().targets = HostsGroup::new(vec![target], false);
            guard.templates.add(Box::new(report));
            guard.templates.set_active("SUSE:Maintenance:1:1");
        }
        sess
    }

    /// N stale sessions whose host close each blocks ~`delay` are all evicted,
    /// and the sweep finishes in ~one wave (≈ `delay`), not ≈ `N × delay`. Also
    /// run at bound `1` to pin that the bound is honoured: the serial run is
    /// demonstrably slower, which is what a regression to a sequential loop
    /// would look like.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sweep_closes_stale_sessions_concurrently_under_the_bound() {
        const N: usize = 6;
        let delay = Duration::from_millis(300);

        // Register N aged-stale slow-closing sessions, then time one sweep at
        // the given bound.
        async fn run_at_bound(n: usize, bound: usize, delay: Duration) -> Duration {
            let reg = reg_with_idle(Duration::from_millis(50));
            let mut sessions = Vec::new();
            for _ in 0..n {
                let sess = slow_close_session(delay).await;
                track(&reg, &sess, mono_millis());
                sessions.push(sess);
            }
            assert_eq!(reg.live_count(), n);
            // Elapse the TTL rather than back-dating; see
            // `sweeper_respects_reactivation_before_close`.
            tokio::time::sleep(Duration::from_millis(120)).await;

            let start = tokio::time::Instant::now();
            sweep_once(&reg.live, reg.idle_timeout, bound).await;
            let elapsed = start.elapsed();
            assert_eq!(reg.live_count(), 0, "all stale sessions must be evicted");
            elapsed
        }

        let concurrent = run_at_bound(N, N, delay).await;
        assert!(
            concurrent < delay * 3,
            "concurrent sweep should finish in ~one wave (<3×delay), took {concurrent:?}"
        );

        let serial = run_at_bound(N, 1, delay).await;
        assert!(
            serial > concurrent * 2,
            "serial (bound=1) sweep {serial:?} must be much slower than concurrent {concurrent:?}"
        );
    }

    /// `close_all` tears down **every** live session regardless of idle state —
    /// the graceful-shutdown path the HTTP transport runs after `axum::serve`
    /// returns.
    #[tokio::test]
    async fn close_all_closes_every_live_session() {
        let reg = reg_with_idle(Duration::from_secs(3600)); // effectively no sweep
        let s1 = slow_close_session(Duration::from_millis(10)).await;
        let s2 = slow_close_session(Duration::from_millis(10)).await;
        track(&reg, &s1, mono_millis());
        track(&reg, &s2, mono_millis());
        assert_eq!(reg.live_count(), 2);

        reg.close_all().await;

        assert_eq!(reg.live_count(), 0, "close_all must empty the live set");
        // Idempotent.
        reg.close_all().await;
        assert_eq!(reg.live_count(), 0);
    }

    /// `close_all` disconnects a live session's hosts, not just idle ones: a busy
    /// session's SSH/pool state must be reclaimed on process shutdown.
    #[tokio::test]
    async fn close_all_disconnects_live_session_hosts() {
        use mtui_hosts::{HostsGroup, MockConnection, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let reg = reg_with_idle(Duration::from_secs(3600));
        let conn = MockConnection::new("h1");
        let handle = conn.clone();
        let target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        let session = reg.make_session();
        {
            let mut guard = session.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
            report.base_mut().targets = HostsGroup::new(vec![target], false);
            guard.templates.add(Box::new(report));
            guard.templates.set_active("SUSE:Maintenance:1:1");
        }
        track(&reg, &session, mono_millis());

        assert!(!handle.is_closed(), "host starts connected");
        reg.close_all().await;
        assert!(
            handle.is_closed(),
            "close_all must disconnect a live session's hosts on shutdown"
        );
        assert_eq!(reg.live_count(), 0);
    }

    #[tokio::test]
    async fn sessions_of_one_registry_share_the_op_lock_holders() {
        let reg = reg_with_idle(Duration::from_secs(3600));
        let a = reg.make_session();
        let b = reg.make_session();
        let other = reg_with_idle(Duration::from_secs(3600)).make_session();

        let a_holders = a.session().lock().await.op_lock_holders().clone();
        let b_holders = b.session().lock().await.op_lock_holders().clone();
        let other_holders = other.session().lock().await.op_lock_holders().clone();
        assert!(a_holders.ptr_eq(&b_holders));
        assert!(!a_holders.ptr_eq(&other_holders));
    }

    /// Cancelling mid-sweep must preempt a wedged teardown batch rather than
    /// block on the stuck close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_preempts_a_wedged_sweep() {
        let reg = reg_with_idle(Duration::from_millis(100));
        let gate = Arc::new(tokio::sync::Notify::new()); // never fired → close wedges
        let sess = wedged_session(Arc::clone(&gate)).await;
        track(&reg, &sess, mono_millis().saturating_sub(10_000));

        let cancel = CancellationToken::new();
        let sweeper = reg.spawn_sweeper(cancel.clone()).unwrap();

        // Let the first cycle wake and enter the wedged teardown, then cancel.
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();

        // Must unwind well within the 45s disconnect budget.
        let joined = tokio::time::timeout(Duration::from_secs(5), sweeper).await;
        assert!(
            joined.is_ok(),
            "cancellation must preempt the wedged teardown"
        );
        gate.notify_waiters(); // release the abandoned close task
    }

    /// Each http-minted server gets its own session, and dropping the server
    /// frees its slot.
    #[tokio::test]
    async fn minted_servers_get_distinct_sessions_and_free_their_slot() {
        let reg = reg_with_idle(Duration::from_secs(3600));
        let a = reg.try_make_server().expect("first http server");
        let b = reg.try_make_server().expect("second http server");

        let sessions = reg.live_sessions();
        assert_eq!(sessions.len(), 2);
        assert!(
            !Arc::ptr_eq(&sessions[0], &sessions[1]),
            "distinct servers must not share a session"
        );

        drop(a);
        assert_eq!(reg.live_count(), 1, "dropped server frees its slot");
        drop(b);
        assert_eq!(reg.live_count(), 0);
    }

    /// A server whose session holds `h1` with an operation lock and a pool
    /// claim, plus the host's mock connection.
    async fn server_holding_h1(reg: &SessionRegistry) -> (McpServer, mtui_hosts::MockConnection) {
        use mtui_hosts::{HostsGroup, MockConnection, Target};
        use mtui_testreport::{ObsReport, TestReport};
        use mtui_types::RequestReviewID;
        use mtui_types::enums::TargetState;

        let conn = MockConnection::new("h1");
        let probe = conn.clone();
        let mut target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
        target.lock("").await.expect("operation lock taken");
        assert!(
            target
                .pool_claim("mtui pool SUSE:Maintenance:1:1 [me]")
                .await
                .expect("pool claim"),
            "pool claim taken"
        );
        let server = reg.try_make_server().expect("http server");
        {
            let session = reg.live_sessions().pop().expect("the minted session");
            let mut guard = session.session().lock().await;
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap());
            report.base_mut().pool_claims.insert("h1".to_owned());
            report.base_mut().targets = HostsGroup::new(vec![target], false);
            guard.templates.add(Box::new(report));
            guard.templates.set_active("SUSE:Maintenance:1:1");
        }
        for path in [mtui_hosts::TARGET_LOCK_PATH, mtui_hosts::POOL_LOCK_PATH] {
            assert!(probe.file_contents(path).is_some(), "fixture holds {path}");
        }
        (server, probe)
    }

    /// Attaches `conn` to `session` with nothing loaded, under an operation
    /// lock, as `add_host` does.
    async fn attach_locked(session: &Arc<McpSession>, conn: mtui_hosts::MockConnection) {
        use mtui_hosts::{Connection as _, Target};
        use mtui_types::enums::TargetState;
        let name = conn.hostname().to_owned();
        let mut target = Target::with_connection(&name, TargetState::Enabled, Box::new(conn));
        target.lock("").await.expect("operation lock taken");
        session.session().lock().await.targets_mut().add(target);
    }

    async fn until_closed(probe: &mtui_hosts::MockConnection) {
        for _ in 0..250 {
            if probe.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A session rmcp drops (client DELETE, keep-alive expiry) is closed: its
    /// operation lock and pool claim are released.
    #[tokio::test]
    async fn dropped_server_closes_its_session() {
        use mtui_hosts::{POOL_LOCK_PATH, TARGET_LOCK_PATH};

        let reg = reg_with_idle(Duration::from_secs(3600));
        let (server, probe) = server_holding_h1(&reg).await;

        drop(server);
        assert_eq!(
            reg.live.lock().unwrap().closing.len(),
            1,
            "the drop spawned one tracked close"
        );
        until_closed(&probe).await;
        assert!(
            probe.is_closed(),
            "the dropped session's host is disconnected"
        );
        for path in [TARGET_LOCK_PATH, POOL_LOCK_PATH] {
            assert!(probe.file_contents(path).is_none(), "{path} released");
        }
    }

    /// A session swept (or shut down) while rmcp kept serving it may attach
    /// hosts again; its drop must still release them.
    #[tokio::test]
    async fn a_swept_session_that_attached_hosts_again_is_closed_on_drop() {
        use mtui_hosts::{Connection as _, MockConnection, TARGET_LOCK_PATH};

        let reg = reg_with_idle(Duration::from_secs(3600));
        let server = reg.try_make_server().expect("http server");
        let session = reg.live_sessions().pop().expect("the minted session");
        reg.close_all().await;
        let conn = MockConnection::new("h2");
        let probe = conn.clone();
        attach_locked(&session, conn).await;
        assert!(probe.file_contents(TARGET_LOCK_PATH).is_some());
        assert!(probe.is_active());
        drop(session);

        drop(server);
        until_closed(&probe).await;
        assert!(probe.is_closed(), "the re-attached host is disconnected");
        assert!(probe.file_contents(TARGET_LOCK_PATH).is_none());
    }

    /// rmcp may drop the server on a thread outside the runtime; the close
    /// still runs on the runtime that minted it.
    #[test]
    fn a_server_dropped_off_its_runtime_is_still_closed() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let reg = reg_with_idle(Duration::from_secs(3600));
        let (server, probe) = rt.block_on(server_holding_h1(&reg));

        drop(server);
        rt.block_on(until_closed(&probe));
        assert!(probe.is_closed(), "closed although dropped off-runtime");
        assert!(probe.file_contents(mtui_hosts::TARGET_LOCK_PATH).is_none());
    }

    /// `Handle::spawn` on a shut-down runtime drops the task; it does not panic.
    #[test]
    fn dropping_a_server_after_its_runtime_shut_down_does_not_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let reg = reg_with_idle(Duration::from_secs(3600));
        let server = rt.block_on(async { reg.try_make_server().expect("http server") });
        drop(rt);
        drop(server);
        assert_eq!(reg.live_count(), 0);
    }

    /// A guard dropped with no runtime (a plain `#[test]`) must not panic.
    #[test]
    fn dropping_a_server_outside_a_runtime_does_not_panic() {
        let reg = reg_with_idle(Duration::from_secs(3600));
        drop(reg.try_make_server().expect("http server"));
        assert_eq!(reg.live_count(), 0);
        assert!(reg.live.lock().unwrap().closing.is_empty());
    }

    /// A sweep cancelled at shutdown has already removed its sessions from the
    /// live set; `close_all` must still see their closes through.
    #[tokio::test]
    async fn a_cancelled_sweep_leaves_its_closes_to_close_all() {
        use mtui_hosts::MockConnection;

        let reg = reg_with_idle(Duration::from_millis(50));
        let conn = MockConnection::new("slow").with_close_delay(Duration::from_millis(300));
        let probe = conn.clone();
        let session = reg.make_session();
        attach_locked(&session, conn).await;
        track(&reg, &session, mono_millis());
        tokio::time::sleep(Duration::from_millis(120)).await;

        tokio::select! {
            () = sweep_once(&reg.live, reg.idle_timeout, 4) => panic!("the sweep must still be closing"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert_eq!(reg.live_count(), 0, "the sweep removed the session");
        assert!(!probe.is_closed());

        reg.close_all().await;
        assert!(
            probe.is_closed(),
            "close_all saw the cancelled sweep's close through"
        );
    }

    /// A guard that drops while `close_all` is waiting is waited for too.
    #[tokio::test]
    async fn close_all_waits_for_a_close_that_starts_meanwhile() {
        use mtui_hosts::MockConnection;

        let reg = reg_with_idle(Duration::from_secs(3600));
        let late = reg.try_make_server().expect("http server");
        let late_session = reg.live_sessions().pop().expect("the minted session");
        let conn = MockConnection::new("late").with_close_delay(Duration::from_millis(600));
        let probe = conn.clone();
        attach_locked(&late_session, conn).await;
        drop(late_session);
        reg.live.lock().unwrap().clear();
        let first = slow_close_session(Duration::from_millis(200)).await;
        track(&reg, &first, mono_millis());

        let closing = tokio::spawn({
            let reg = reg.clone();
            async move { reg.close_all().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(late);
        closing.await.expect("close_all joins");
        assert!(
            probe.is_closed(),
            "close_all returned before the late close finished"
        );
    }

    /// Guard closes run at most `[mcp] sweep_parallel` at a time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn guard_closes_are_bounded_by_sweep_parallel() {
        use mtui_hosts::MockConnection;

        let mut config = Config::default();
        config.mcp_sweep_parallel = 1;
        let reg = SessionRegistry::new(Arc::new(mtui_core::register_all()), config);
        let mut probes = Vec::new();
        let mut servers = Vec::new();
        for name in ["a", "b"] {
            let server = reg.try_make_server().expect("http server");
            let session = reg
                .live_sessions()
                .into_iter()
                .find(|s| s.session().try_lock().is_ok_and(|g| g.targets().is_empty()))
                .expect("the new session");
            let conn = MockConnection::new(name).with_close_delay(Duration::from_millis(300));
            probes.push(conn.clone());
            attach_locked(&session, conn).await;
            servers.push(server);
        }
        drop(servers);
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(
            probes.iter().filter(|p| p.is_closed()).count(),
            1,
            "one close at a time"
        );
        for probe in &probes {
            until_closed(probe).await;
            assert!(probe.is_closed());
        }
    }
}
