//! Session resolution seam: [`SessionProvider`] + the stdio [`StdioProvider`],
//! plus the http [`SessionRegistry`] factory with its cap + idle-TTL enforcement.
//!
//! The tool layer resolves the [`McpSession`] for each call through a
//! [`SessionProvider`], so it never cares which transport it runs under. Both
//! implementers answer the same `get_or_create(key)`, which is why the trait —
//! not a concrete session — is the seam:
//!
//! - **stdio** ([`StdioProvider`]) — one process, one client, one session for
//!   every call (the `key` is ignored).
//! - **http** — a fresh isolated session per client, bound by the
//!   [`SessionRegistry`] *factory* (`try_make_server`), which rmcp's
//!   streamable-HTTP transport calls once per new MCP session; rmcp's session
//!   manager owns the `Mcp-Session-Id` keying and the transport teardown.
//!
//! ## Cap + idle-TTL
//!
//! rmcp 3.x has no max-sessions or idle-TTL knob, its `service_factory` receives
//! **no** session key, and rmcp owns teardown with no application hook. So the
//! `[mcp] session_cap` / `session_idle_timeout` bounds are enforced
//! **application-side, wrapped around the factory** rather than by mirroring
//! rmcp's session map:
//!
//! * the **cap**: [`SessionRegistry::try_make_server`] refuses a new session past
//!   `session_cap` with an [`io::Error`], which rmcp surfaces as an
//!   internal-error HTTP response — a bounded DoS refusal instead of an
//!   unbounded fleet of SSH-`targets`-holding sessions;
//! * the idle **sweeper** ([`SessionRegistry::spawn_sweeper`]): evicts +
//!   [`McpSession::close`]-es any session untouched for `session_idle_timeout`
//!   seconds (reclaiming its SSH host connections); `0` disables it.
//!
//! Each minted [`McpServer`] carries a [`SessionGuard`] that removes the session
//! from the live set on `Drop` (so a session rmcp tears down frees a cap slot
//! with no teardown hook) and shares the last-touch timestamp [`McpServer`] bumps
//! per tool call. Activity is therefore measured at the *tool-call* boundary;
//! pure SSE-GET keepalive traffic (owned by rmcp, invisible here) is not counted
//! — acceptable for a DoS/idle guard.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use mtui_config::Config;
use mtui_core::{HOST_CLOSE_TIMEOUT, Registry};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::server::McpServer;
use crate::session::McpSession;

/// A monotonic clock reading in milliseconds, for last-touch bookkeeping.
///
/// Measured against a process-lifetime epoch so the value is a plain comparable
/// `u64` shareable through an [`AtomicU64`] (an `Instant` is not atomically
/// storable). Only *differences* are meaningful.
pub(crate) fn now_millis() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_millis() as u64
}

/// The minimal surface the tool layer resolves a session through.
///
/// Maps a per-client `key` to the [`McpSession`] the call dispatches against,
/// minting one on first use where applicable.
///
/// A native `async fn` is fine here: the trait is consumed by a concrete
/// provider type (the rmcp `ServerHandler` is not `dyn`-compatible anyway), so
/// no `dyn SessionProvider` boxing is required.
pub trait SessionProvider {
    /// Returns the session bound to `key`, minting one if needed. Single-session
    /// providers (stdio) ignore `key` and always return the same session.
    fn get_or_create(&self, key: &str) -> impl Future<Output = Arc<McpSession>> + Send;
}

/// The stdio single-session provider: one [`McpSession`] reused for every call.
///
/// One stdio process serves exactly one client, so there is no per-client
/// isolation to do and `get_or_create` returns the same session for any `key`.
#[derive(Clone)]
pub struct StdioProvider {
    session: Arc<McpSession>,
}

impl StdioProvider {
    /// Builds the provider's single headless session from `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            session: McpSession::new(config),
        }
    }
}

impl SessionProvider for StdioProvider {
    async fn get_or_create(&self, _key: &str) -> Arc<McpSession> {
        // The key is intentionally ignored; per-client keying is the http
        // registry's job.
        Arc::clone(&self.session)
    }
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

/// The registry's shared live-session table, keyed by a monotonic session id.
type LiveSet = Arc<StdMutex<HashMap<u64, TrackedSession>>>;

/// RAII handle owned by each minted [`McpServer`]: unregisters its session on
/// `Drop`.
///
/// When rmcp drops the `McpServer` at session close (or the idle sweeper evicts
/// it), this frees a `session_cap` slot automatically, with no rmcp teardown
/// hook required. Idempotent by construction (a removed key is a no-op remove).
pub struct SessionGuard {
    live: LiveSet,
    id: u64,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.live.lock() {
            set.remove(&self.id);
        }
    }
}

/// The http per-client session factory with cap + idle-TTL enforcement.
///
/// Under `--transport http` one process serves many concurrent clients, each of
/// which must see **only its own** loaded template + SSH `targets` — sharing one
/// session would let one client's `load_template` clobber another's. So
/// [`try_make_server`](Self::try_make_server), the closure rmcp's
/// `StreamableHttpService` invokes once per session, mints a **fresh, fully
/// isolated** [`McpServer`] with its own [`McpSession`].
///
/// The live set holds [`Weak`] handles rather than owning the session map (rmcp
/// owns that, keyed by `Mcp-Session-Id`), and carries the cap + idle-TTL bounds
/// described in the module docs.
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
    /// Monotonic session-id counter (each mint gets a fresh id).
    next_id: Arc<AtomicU64>,
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
            live: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        }
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
        self.live
            .lock()
            .map(|s| s.values().filter(|t| t.session.strong_count() > 0).count())
            .unwrap_or(0)
    }

    /// Strong handles to every live session, for inspection: upgrades each
    /// tracked [`Weak`], skipping the dead. Lets callers (and the sweeper tests)
    /// observe a session's teardown after an eviction.
    #[must_use]
    pub fn live_sessions(&self) -> Vec<Arc<McpSession>> {
        self.live
            .lock()
            .map(|s| s.values().filter_map(|t| t.session.upgrade()).collect())
            .unwrap_or_default()
    }

    /// Mint a fresh [`McpSession`], cloning the base [`Config`] so its mutable
    /// scalar state is independent (own `metadata` / `targets` / capture sink).
    /// The isolation boundary: [`try_make_server`](Self::try_make_server) wraps
    /// it for the transport, and tests use it directly.
    #[must_use]
    pub fn make_session(&self) -> Arc<McpSession> {
        McpSession::new(self.config.clone())
    }

    /// Mint a fresh, isolated, cap-checked [`McpServer`] for one MCP session.
    ///
    /// Called once per new session by the streamable-HTTP transport's
    /// `service_factory`. Refuses to mint past [`cap`](Self::cap) with an
    /// [`io::Error`] rmcp surfaces as an internal-error response (a bounded DoS
    /// refusal). On success it registers the session's [`Weak`] handle + a
    /// shared last-touch timestamp, and hands the [`McpServer`] a
    /// [`SessionGuard`] (frees the slot on `Drop`) plus that last-touch handle.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] of kind [`io::ErrorKind::Other`] when the live
    /// set already holds [`cap`](Self::cap) sessions.
    pub fn try_make_server(&self) -> io::Result<McpServer> {
        let mut set = self
            .live
            .lock()
            .map_err(|_| io::Error::other("session registry lock poisoned"))?;

        // Drop dead entries so a burst of disconnects+reconnects cannot
        // spuriously trip the cap.
        set.retain(|_, t| t.session.strong_count() > 0);

        if set.len() >= self.cap {
            return Err(io::Error::other(format!(
                "session registry full: {} concurrent client sessions already \
                 active; retry once a session is released or raise \
                 [mcp] session_cap",
                self.cap
            )));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session = self.make_session();
        let last_touch = Arc::new(AtomicU64::new(now_millis()));
        set.insert(
            id,
            TrackedSession {
                session: Arc::downgrade(&session),
                last_touch: Arc::clone(&last_touch),
            },
        );
        drop(set);

        let guard = SessionGuard {
            live: Arc::clone(&self.live),
            id,
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
    /// Cancelling the idle sweeper alone does **not** release live sessions (its
    /// cancel branch just returns, and the registry holds only [`Weak`] handles
    /// whose `Drop` cannot run the async pool-claim release), so a clean Ctrl-C /
    /// SIGTERM of a busy server would otherwise leak every active session's pool
    /// claims and SSH connections.
    ///
    /// Closes concurrently under [`sweep_parallel`](Self::sweep_parallel) with a
    /// single overall deadline so one wedged host close cannot hang process exit.
    /// Best-effort and idempotent: a second call finds an empty set.
    pub(crate) async fn close_all(&self) {
        let sessions: Vec<Arc<McpSession>> = {
            let mut set = match self.live.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            let sessions = set.values().filter_map(|t| t.session.upgrade()).collect();
            set.clear();
            sessions
        };
        close_sessions(sessions, self.sweep_parallel).await;
    }
}

/// Close a batch of sessions concurrently under `parallel`, bounded by a single
/// overall deadline (`HOST_CLOSE_TIMEOUT + 1s`).
///
/// Shared by the idle sweep ([`sweep_once`]) and graceful shutdown
/// ([`SessionRegistry::close_all`]). One budget, not N×, so the batch returns
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
    let mut set = match live.lock() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    set.retain(|_, t| t.session.strong_count() > 0);
    let timeout_ms = timeout.as_millis() as u64;
    set.iter()
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
    let now = now_millis();
    let timeout_ms = timeout.as_millis() as u64;

    // Phase 1 (serial, lock-held): the re-read and the removal run with no await
    // between them, so a client refresh cannot slip into that gap. Only sessions
    // removed here are closed, so a spared session is never torn down.
    let mut to_close: Vec<Arc<McpSession>> = Vec::new();
    for (id, session) in collect_stale(live, timeout, now) {
        let mut set = match live.lock() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(tracked) = set.get(&id) else {
            continue; // already evicted elsewhere
        };
        let touched = tracked.last_touch.load(Ordering::Relaxed);
        if now_millis().saturating_sub(touched) < timeout_ms {
            tracing::info!(id, "skipping sweep: session re-activated");
            continue;
        }
        set.remove(&id);
        drop(set);
        tracing::info!(id, "sweeping idle MCP session");
        to_close.push(session);
    }
    if to_close.is_empty() {
        return;
    }

    // Phase 2 (concurrent, unlocked). Entries are already out of the live set,
    // so an abandoned close is a best-effort no-op the OS reclaims at exit.
    close_sessions(to_close, parallel).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdio provider is single-entry: any two keys resolve to the same
    /// session instance.
    #[tokio::test]
    async fn stdio_provider_returns_same_session_for_any_key() {
        let provider = StdioProvider::new(Config::default());

        let a = provider.get_or_create("client-a").await;
        let b = provider.get_or_create("client-b").await;

        assert!(
            Arc::ptr_eq(&a, &b),
            "stdio provider must return the same session for different keys"
        );
    }

    /// The resolved session exposes the guarded [`Session`] and capture sink the
    /// dispatch path needs, the sink starting empty.
    #[tokio::test]
    async fn resolved_session_exposes_dispatch_seams() {
        let provider = StdioProvider::new(Config::default());
        let session = provider.get_or_create("<default>").await;

        let _guard = session.session().lock().await;
        assert_eq!(session.output().take(), "");
    }

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
        reg.live.lock().unwrap().insert(
            id,
            TrackedSession {
                session: Arc::downgrade(session),
                last_touch: Arc::clone(&last_touch),
            },
        );
        (id, last_touch)
    }

    #[tokio::test]
    async fn sweeper_evicts_stale_session() {
        let reg = reg_with_idle(Duration::from_millis(200));
        let session = reg.make_session();
        // The sweeper's first cycle (>= 1s later) sees it aged past the 200ms TTL.
        let (_id, _touch) = track(&reg, &session, now_millis());
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
        track(&reg, &session, now_millis());

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
        let (_id, touch) = track(&reg, &session, now_millis());

        let cancel = CancellationToken::new();
        let sweeper = reg.spawn_sweeper(cancel.clone()).unwrap();

        // Keep touching under the TTL for ~600ms (several sweep cycles).
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            touch.store(now_millis(), Ordering::Relaxed);
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
        let (id, touch) = track(&reg, &session, now_millis());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let now = now_millis();
        let stale = collect_stale(&reg.live, reg.idle_timeout, now);
        assert_eq!(stale.len(), 1, "aged session is stale");

        // A client re-activates it before the sweep evicts.
        touch.store(now_millis(), Ordering::Relaxed);

        // Drive the loop's per-entry re-check manually.
        let timeout_ms = reg.idle_timeout.as_millis() as u64;
        {
            let set = reg.live.lock().unwrap();
            let tracked = set.get(&id).unwrap();
            let touched = tracked.last_touch.load(Ordering::Relaxed);
            assert!(
                now_millis().saturating_sub(touched) < timeout_ms,
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
                track(&reg, &sess, now_millis());
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
        track(&reg, &s1, now_millis());
        track(&reg, &s2, now_millis());
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
        track(&reg, &session, now_millis());

        assert!(!handle.is_closed(), "host starts connected");
        reg.close_all().await;
        assert!(
            handle.is_closed(),
            "close_all must disconnect a live session's hosts on shutdown"
        );
        assert_eq!(reg.live_count(), 0);
    }

    /// Cancelling mid-sweep must preempt a wedged teardown batch rather than
    /// block on the stuck close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_preempts_a_wedged_sweep() {
        let reg = reg_with_idle(Duration::from_millis(100));
        let gate = Arc::new(tokio::sync::Notify::new()); // never fired → close wedges
        let sess = wedged_session(Arc::clone(&gate)).await;
        track(&reg, &sess, now_millis().saturating_sub(10_000));

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
}
