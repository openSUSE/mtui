//! In-process arbitration of reference hosts across loaded templates.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/host_arbiter.py` (`HostArbiter`,
//! `get_arbiter`) and its `tests/test_host_arbiter.py`.
//!
//! When several templates are loaded in one process (one REPL, or several MCP
//! sessions sharing an interpreter) and fan-out connects them concurrently, the
//! remote `/var/lock/mtui.lock` cannot keep two templates off the same shared
//! host: that lock is keyed on `(user, pid)` and every same-process template
//! shares the pid, so [`TargetLock::is_mine`](super::locks::TargetLock) is true
//! for all of them (upstream RFC §5.7).
//!
//! [`HostArbiter`] closes that gap. It is a task-safe map `hostname -> owner`
//! where the owner is the composite [`Owner`] `(registry_id, RRID)`, so two MCP
//! sessions loading the **same** RRID are still distinct owners. A claim that
//! finds every candidate held queues until one is released or the wait budget
//! (`[lock] wait`) expires.
//!
//! The arbiter dedups *claims* (which template gets which host), not
//! *connections*: each template still owns its own [`Target`](super::Target)
//! and SSH session. A single process-global instance is shared by every caller;
//! obtain it via [`get_arbiter`].
//!
//! ## Concurrency model (deviation from upstream)
//!
//! Upstream blocks a worker thread on a `threading.Condition`. This port is
//! async-native: [`HostArbiter::acquire_any`] is `async`, the queue wakes via a
//! [`tokio::sync::Notify`], and the wait budget is driven by an injected
//! [`Clock`] (the same trait [`TargetLock`](super::locks::TargetLock) uses), so
//! timeout behaviour is deterministic and fast under a fake clock in tests. The
//! process-global singleton uses [`std::sync::OnceLock`] in place of upstream's
//! double-checked `_ARBITER` / `_ARBITER_LOCK`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::Notify;

use super::locks::{Clock, SystemClock};

/// Owner key: `(registry_id, rrid)`.
///
/// Two sessions loading the same RRID under different registries are distinct
/// owners, so the arbiter keys on the pair, not the RRID alone.
pub type Owner = (String, String);

/// The default per-wake-up poll ceiling, in seconds (upstream `poll=15`).
const DEFAULT_POLL: u64 = 15;

/// Task-safe `hostname -> owner` map with an async wait queue.
///
/// One instance per process, shared across callers (see [`get_arbiter`]). All
/// methods are safe to call concurrently from the tasks that fan-out connects
/// spawn.
pub struct HostArbiter<C: Clock = SystemClock> {
    owners: Mutex<HashMap<String, Owner>>,
    /// Wakes queued [`acquire_any`](Self::acquire_any) callers on any release.
    notify: Notify,
    clock: C,
}

impl Default for HostArbiter<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl HostArbiter<SystemClock> {
    /// Create an empty arbiter backed by the real [`SystemClock`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl<C: Clock> HostArbiter<C> {
    /// Create an empty arbiter with an injected [`Clock`] (used by tests).
    #[must_use]
    pub fn with_clock(clock: C) -> Self {
        Self {
            owners: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            clock,
        }
    }

    /// Claim `host` for `owner` if it is free or already ours.
    ///
    /// Returns `true` if `owner` now holds `host` (idempotent when it already
    /// did); `false` if another owner holds it.
    #[must_use]
    pub fn try_acquire(&self, host: &str, owner: &Owner) -> bool {
        let mut owners = self.owners.lock().expect("arbiter mutex poisoned");
        match owners.get(host) {
            Some(held) if held != owner => false,
            _ => {
                owners.insert(host.to_owned(), owner.clone());
                true
            }
        }
    }

    /// Claim one free host from `candidates` for `owner`.
    ///
    /// Tries each candidate in order. If all are held by other owners and
    /// `wait > 0`, queues until a candidate is released (waking at most every
    /// `poll` seconds, `poll <= 0` defaults to 15) and retries, up to `wait`
    /// seconds total. `wait <= 0` fails fast. Empty `candidates` returns `None`.
    ///
    /// Returns the claimed hostname, or `None` if none could be claimed within
    /// the wait budget.
    pub async fn acquire_any(
        &self,
        candidates: &[String],
        owner: &Owner,
        wait: i64,
        poll: i64,
    ) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let poll = if poll > 0 { poll as u64 } else { DEFAULT_POLL } as f64;
        let deadline = if wait > 0 {
            Some(self.clock.monotonic() + wait as f64)
        } else {
            None
        };
        let mut warned = false;
        loop {
            // Register interest *before* checking, so a release that races with
            // this check still wakes us on the next `notified().await`.
            let notified = self.notify.notified();

            if let Some(host) = self.try_claim_first(candidates, owner) {
                return Some(host);
            }
            let Some(deadline) = deadline else {
                return None; // fail-fast: wait <= 0
            };
            let remaining = deadline - self.clock.monotonic();
            if remaining <= 0.0 {
                tracing::warn!(
                    ?owner,
                    wait,
                    "host pool exhausted; gave up after the wait budget"
                );
                return None;
            }
            if !warned {
                tracing::warn!(
                    ?owner,
                    wait,
                    "host pool busy; waiting up to the wait budget for a free host"
                );
                warned = true;
            }
            // Wake on either a release notification or the poll timeout,
            // whichever comes first. Under a fake clock, `sleep` returns
            // instantly and advances mock time, keeping the loop deterministic.
            tokio::select! {
                () = notified => {}
                () = self.clock.sleep(poll.min(remaining)) => {}
            }
        }
    }

    /// Try to claim the first free (or already-ours) candidate. Holds the lock
    /// for the whole scan so the claim is atomic.
    fn try_claim_first(&self, candidates: &[String], owner: &Owner) -> Option<String> {
        let mut owners = self.owners.lock().expect("arbiter mutex poisoned");
        for host in candidates {
            match owners.get(host) {
                Some(held) if held != owner => {}
                _ => {
                    owners.insert(host.clone(), owner.clone());
                    return Some(host.clone());
                }
            }
        }
        None
    }

    /// Return the owner currently holding `host`, or `None`.
    #[must_use]
    pub fn owner_of(&self, host: &str) -> Option<Owner> {
        self.owners
            .lock()
            .expect("arbiter mutex poisoned")
            .get(host)
            .cloned()
    }

    /// Release `host` if held by `owner`, waking any waiters.
    pub fn release(&self, host: &str, owner: &Owner) {
        let mut owners = self.owners.lock().expect("arbiter mutex poisoned");
        if owners.get(host) == Some(owner) {
            owners.remove(host);
            drop(owners);
            self.notify.notify_waiters();
        }
    }

    /// Release every host held by `owner`, waking any waiters.
    pub fn release_owner(&self, owner: &Owner) {
        let mut owners = self.owners.lock().expect("arbiter mutex poisoned");
        let freed: Vec<String> = owners
            .iter()
            .filter(|(_, o)| *o == owner)
            .map(|(h, _)| h.clone())
            .collect();
        for h in &freed {
            owners.remove(h);
        }
        drop(owners);
        if !freed.is_empty() {
            self.notify.notify_waiters();
        }
    }
}

/// The process-global [`HostArbiter`] (created on first use).
///
/// The Rust-idiomatic replacement for upstream's `_ARBITER` / `_ARBITER_LOCK`
/// double-checked singleton. Pinned to [`SystemClock`]; tests construct their
/// own [`HostArbiter::with_clock`] instead of touching the global.
#[must_use]
pub fn get_arbiter() -> &'static HostArbiter<SystemClock> {
    static ARBITER: OnceLock<HostArbiter<SystemClock>> = OnceLock::new();
    ARBITER.get_or_init(HostArbiter::new)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A fake clock mirroring `locks.rs`: fixed `now`, a manually-advanceable
    /// monotonic counter (ms), and a no-op sleep that advances the monotonic
    /// clock by the slept amount — so wait budgets elapse instantly.
    #[derive(Clone)]
    struct FakeClock {
        mono: Arc<AtomicU64>, // milliseconds
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                mono: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl Clock for FakeClock {
        fn now_unix(&self) -> u64 {
            1_700_000_000
        }
        fn monotonic(&self) -> f64 {
            self.mono.load(Ordering::SeqCst) as f64 / 1000.0
        }
        async fn sleep(&self, secs: f64) {
            self.mono
                .fetch_add((secs * 1000.0) as u64, Ordering::SeqCst);
        }
    }

    fn owner_a() -> Owner {
        ("reg1".into(), "SUSE:Maintenance:1:1".into())
    }

    /// Same RRID, different registry — a distinct owner.
    fn owner_b() -> Owner {
        ("reg2".into(), "SUSE:Maintenance:1:1".into())
    }

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn fake_arb() -> HostArbiter<FakeClock> {
        HostArbiter::with_clock(FakeClock::new())
    }

    #[tokio::test]
    async fn try_acquire_free_then_foreign() {
        let arb = fake_arb();
        assert!(arb.try_acquire("host1", &owner_a()));
        // same owner re-acquires idempotently
        assert!(arb.try_acquire("host1", &owner_a()));
        // different owner is refused
        assert!(!arb.try_acquire("host1", &owner_b()));
        assert_eq!(arb.owner_of("host1"), Some(owner_a()));
    }

    #[tokio::test]
    async fn acquire_any_picks_first_free() {
        let arb = fake_arb();
        assert!(arb.try_acquire("h1", &owner_b()));
        let got = arb
            .acquire_any(&hosts(&["h1", "h2", "h3"]), &owner_a(), 0, 15)
            .await;
        assert_eq!(got.as_deref(), Some("h2"));
        assert_eq!(arb.owner_of("h2"), Some(owner_a()));
    }

    #[tokio::test]
    async fn acquire_any_all_busy_failfast() {
        let arb = fake_arb();
        assert!(arb.try_acquire("h1", &owner_b()));
        assert_eq!(
            arb.acquire_any(&hosts(&["h1"]), &owner_a(), 0, 15).await,
            None
        );
    }

    #[tokio::test]
    async fn acquire_any_empty_candidates() {
        let arb = fake_arb();
        assert_eq!(arb.acquire_any(&[], &owner_a(), 5, 15).await, None);
    }

    #[tokio::test]
    async fn release_wakes_waiter() {
        // Uses the real SystemClock so the Notify wake races a real (short)
        // wait budget, exercising the async wake path rather than mock time.
        let arb = Arc::new(HostArbiter::new());
        assert!(arb.try_acquire("h1", &owner_b()));

        let waiter_arb = Arc::clone(&arb);
        let waiter = tokio::spawn(async move {
            waiter_arb
                .acquire_any(&hosts(&["h1"]), &owner_a(), 5, 1)
                .await
        });

        // Let the waiter block on the empty pool.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        arb.release("h1", &owner_b());

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), waiter)
            .await
            .expect("waiter did not finish in time")
            .expect("waiter task panicked");
        assert_eq!(result.as_deref(), Some("h1"));
        assert_eq!(arb.owner_of("h1"), Some(owner_a()));
    }

    #[tokio::test]
    async fn acquire_any_times_out() {
        // Fake clock: the wait budget elapses via instant mock sleeps, so this
        // returns None without any real delay.
        let arb = fake_arb();
        assert!(arb.try_acquire("h1", &owner_b()));
        let start = arb.clock.monotonic();
        let got = arb.acquire_any(&hosts(&["h1"]), &owner_a(), 1, 1).await;
        assert_eq!(got, None);
        assert!(arb.clock.monotonic() - start >= 1.0);
    }

    #[tokio::test]
    async fn release_owner_frees_all() {
        let arb = fake_arb();
        assert!(arb.try_acquire("h1", &owner_a()));
        assert!(arb.try_acquire("h2", &owner_a()));
        assert!(arb.try_acquire("h3", &owner_b()));
        arb.release_owner(&owner_a());
        assert_eq!(arb.owner_of("h1"), None);
        assert_eq!(arb.owner_of("h2"), None);
        assert_eq!(arb.owner_of("h3"), Some(owner_b()));
    }

    #[tokio::test]
    async fn release_only_when_owner_matches() {
        let arb = fake_arb();
        assert!(arb.try_acquire("h1", &owner_a()));
        arb.release("h1", &owner_b()); // not the holder; no-op
        assert_eq!(arb.owner_of("h1"), Some(owner_a()));
    }

    #[tokio::test]
    async fn get_arbiter_singleton() {
        let a = get_arbiter();
        let b = get_arbiter();
        assert!(std::ptr::eq(a, b));
    }
}
