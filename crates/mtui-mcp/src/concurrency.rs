//! Async concurrency primitives for the MCP session's per-template locking.
//!
//! Port of upstream `mtui.mcp.session._RWLock`: a minimal async readers-writer
//! lock used as the **registry gate**. Many *shared* holders (per-RRID commands,
//! each mutating only its own template) may run at once, but a *shared* holder
//! excludes every *exclusive* holder and vice-versa. Exclusive holders (registry
//! mutators — `load_template` / `unload` — and unscoped fan-out) run one at a
//! time with no shared holder present.
//!
//! **Writer-preference is intentional** (upstream `_writer_waiting`): while an
//! exclusive waiter is pending, new shared acquisitions block, so a steady
//! stream of per-RRID commands cannot starve a `load_template`. There is no
//! fairness queue beyond that; the single-session workload (a handful of
//! concurrent subagents) does not need one.
//!
//! ## Locking depth (beads `mtui-rs-76e.11` + `mtui-rs-f36r` / `mtui-rs-0mop.11`)
//!
//! This gate + the per-RRID lock map in [`crate::session`] land the lock
//! *discipline*: same-RRID and unscoped calls serialise, and registry mutators
//! drain in-flight per-RRID work. Genuine wall-clock concurrency between
//! *different-RRID* calls **has landed** (`mtui-rs-f36r`): `mtui-core` report
//! entries are per-entry `Arc<Mutex<..>>`, and a single-real-RRID call dispatches
//! on a [`Session::fork_for_call`](mtui_core::Session::fork_for_call) (sharing
//! the entry locks, with its own display) via
//! [`dispatch_command`](mtui_core::dispatch_command), so
//! [`run_command`](crate::McpSession::run_command) no longer holds a session-wide
//! mutex across dispatch. Two different-RRID calls now acquire distinct per-RRID
//! locks *and* run in parallel; registry-structure mutators still take this gate
//! *exclusive* against the canonical session.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// State shared between all holders/waiters of an [`RwGate`].
#[derive(Default)]
struct Inner {
    /// Number of active shared (reader) holders.
    readers: usize,
    /// Number of pending or active exclusive (writer) holders. Non-zero blocks
    /// new shared acquisitions (writer preference).
    writers_waiting: usize,
    /// `true` while an exclusive holder owns the gate.
    writer_active: bool,
}

/// A minimal async writer-preference readers-writer gate.
///
/// Cloneable: all clones share one underlying state, so an [`RwGate`] handed to
/// several tasks gates them against one another. Acquisition returns an RAII
/// guard whose `Drop` releases the hold and wakes waiters.
#[derive(Clone, Default)]
pub struct RwGate {
    inner: Arc<Mutex<Inner>>,
    /// Woken on every release so blocked acquirers re-check their condition.
    notify: Arc<Notify>,
}

impl RwGate {
    /// Builds a fresh, unheld gate.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Acquires the gate in shared (reader) mode, waiting out any active or
    /// pending exclusive holder (writer preference).
    ///
    /// The returned [`SharedGuard`] releases the hold on drop.
    pub(crate) async fn shared(&self) -> SharedGuard {
        loop {
            // Register for a wakeup *before* checking so we cannot miss a release
            // that happens between the check and the await. `enable()` arms the
            // `Notified` future as a registered waiter without awaiting it, so a
            // `notify_waiters()` fired after the condition check but before the
            // `.await` below is still delivered. The std mutex is held only for
            // the counter check/bump — never across an await.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock().expect("rw gate poisoned");
                if inner.writers_waiting == 0 && !inner.writer_active {
                    inner.readers += 1;
                    return SharedGuard {
                        inner: Arc::clone(&self.inner),
                        notify: Arc::clone(&self.notify),
                    };
                }
            }
            notified.await;
        }
    }

    /// Acquires the gate in exclusive (writer) mode, waiting for every shared
    /// holder to drain and no other writer to be active.
    ///
    /// The returned [`ExclusiveGuard`] releases the hold on drop. `writers_waiting`
    /// is bumped for the whole wait so pending shared acquisitions block behind
    /// this writer.
    ///
    /// **Cancellation-safe:** the `writers_waiting` bump is owned by a
    /// [`PendingWriter`] RAII guard, so dropping this future while parked (e.g. a
    /// cancelled MCP request) restores the counter and wakes waiters rather than
    /// leaking the bump and deadlocking readers (`mtui-rs-b8yi`). On success the
    /// pending guard is disarmed and the count is handed to the [`ExclusiveGuard`],
    /// which decrements it on its own drop — so success-path accounting is
    /// unchanged.
    pub(crate) async fn exclusive(&self) -> ExclusiveGuard {
        let mut pending = PendingWriter::new(Arc::clone(&self.inner), Arc::clone(&self.notify));
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock().expect("rw gate poisoned");
                if inner.readers == 0 && !inner.writer_active {
                    inner.writer_active = true;
                    // Hand the `writers_waiting` bump to the ExclusiveGuard; it
                    // will decrement on its own drop.
                    pending.armed = false;
                    return ExclusiveGuard {
                        inner: Arc::clone(&self.inner),
                        notify: Arc::clone(&self.notify),
                    };
                }
            }
            notified.await;
        }
    }
}

/// RAII guard owning the `writers_waiting` bump of a *pending* (not-yet-acquired)
/// writer.
///
/// Constructed the moment [`RwGate::exclusive`] starts waiting; if that future is
/// dropped before it acquires (cancellation), `drop` restores the counter and
/// wakes waiters. On success it is disarmed and the bump is inherited by the
/// [`ExclusiveGuard`], so the counter is decremented exactly once either way.
struct PendingWriter {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
    /// `true` while this guard owns the `writers_waiting` bump.
    armed: bool,
}

impl PendingWriter {
    fn new(inner: Arc<Mutex<Inner>>, notify: Arc<Notify>) -> Self {
        inner.lock().expect("rw gate poisoned").writers_waiting += 1;
        Self {
            inner,
            notify,
            armed: true,
        }
    }
}

impl Drop for PendingWriter {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut inner = self.inner.lock().expect("rw gate poisoned");
            inner.writers_waiting -= 1;
        }
        self.notify.notify_waiters();
    }
}

/// RAII guard for a shared (reader) hold on an [`RwGate`].
#[must_use = "the shared hold is released as soon as the guard is dropped"]
pub struct SharedGuard {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

impl Drop for SharedGuard {
    fn drop(&mut self) {
        {
            let mut inner = self.inner.lock().expect("rw gate poisoned");
            inner.readers -= 1;
        }
        self.notify.notify_waiters();
    }
}

/// RAII guard for an exclusive (writer) hold on an [`RwGate`].
#[must_use = "the exclusive hold is released as soon as the guard is dropped"]
pub struct ExclusiveGuard {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        {
            let mut inner = self.inner.lock().expect("rw gate poisoned");
            inner.writer_active = false;
            inner.writers_waiting -= 1;
        }
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Many shared holders coexist: two `shared()` acquisitions are both live at
    /// once (readers reaches 2).
    #[tokio::test]
    async fn shared_holders_coexist() {
        let gate = RwGate::new();
        let a = gate.shared().await;
        let b = gate.shared().await;
        {
            let inner = gate.inner.lock().unwrap();
            assert_eq!(inner.readers, 2, "both shared holders live");
        }
        drop(a);
        drop(b);
    }

    /// An exclusive holder excludes shared holders: while exclusive is held, a
    /// `shared()` call does not complete; it proceeds only after release.
    #[tokio::test]
    async fn exclusive_excludes_shared() {
        let gate = RwGate::new();
        let excl = gate.exclusive().await;

        let g2 = gate.clone();
        let acquired = Arc::new(AtomicUsize::new(0));
        let acquired2 = Arc::clone(&acquired);
        let handle = tokio::spawn(async move {
            let _s = g2.shared().await;
            acquired2.store(1, Ordering::SeqCst);
        });

        // Give the spawned task time to (fail to) acquire.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            0,
            "shared must not acquire while exclusive is held"
        );

        drop(excl);
        handle.await.unwrap();
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            1,
            "shared acquires after exclusive release"
        );
    }

    /// Writer preference: a pending exclusive waiter blocks a new shared
    /// acquisition even while an existing shared holder is live, so the writer
    /// cannot be starved.
    #[tokio::test]
    async fn writer_preference_blocks_new_shared() {
        let gate = RwGate::new();
        let held = gate.shared().await; // one reader live

        // A writer starts waiting (readers == 1, so it blocks).
        let gw = gate.clone();
        let writer_done = Arc::new(AtomicUsize::new(0));
        let wd = Arc::clone(&writer_done);
        let writer = tokio::spawn(async move {
            let _e = gw.exclusive().await;
            wd.store(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Now a new shared acquisition must block behind the pending writer.
        let gs = gate.clone();
        let shared_done = Arc::new(AtomicUsize::new(0));
        let sd = Arc::clone(&shared_done);
        let reader2 = tokio::spawn(async move {
            let _s = gs.shared().await;
            sd.store(1, Ordering::SeqCst);
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            shared_done.load(Ordering::SeqCst),
            0,
            "new shared must block behind a pending writer"
        );
        assert_eq!(
            writer_done.load(Ordering::SeqCst),
            0,
            "writer still blocked by the live reader"
        );

        // Release the original reader: the writer runs first, then the reader.
        drop(held);
        writer.await.unwrap();
        assert_eq!(writer_done.load(Ordering::SeqCst), 1, "writer ran");
        reader2.await.unwrap();
        assert_eq!(shared_done.load(Ordering::SeqCst), 1, "second reader ran");
    }

    /// Cancelling a *waiting* writer (its `exclusive()` future is dropped before
    /// it ever acquires) must not leak `writers_waiting`, or new shared
    /// acquisitions would be blocked forever. Regression for `mtui-rs-b8yi`.
    #[tokio::test]
    async fn cancelled_writer_does_not_block_readers() {
        let gate = RwGate::new();
        let held = gate.shared().await; // one reader live -> writer must park

        // A writer starts waiting; it parks because readers == 1.
        let gw = gate.clone();
        let writer = tokio::spawn(async move {
            let _e = gw.exclusive().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Cancel the waiting writer before it ever acquires.
        writer.abort();
        let _ = writer.await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Release the reader and prove a fresh shared acquisition completes
        // promptly rather than deadlocking behind the leaked writer count.
        drop(held);
        let g = gate.clone();
        tokio::time::timeout(Duration::from_millis(500), async move {
            let _s = g.shared().await;
        })
        .await
        .expect("shared must acquire after a cancelled writer");

        let inner = gate.inner.lock().unwrap();
        assert_eq!(
            inner.writers_waiting, 0,
            "cancelled writer must not leak writers_waiting"
        );
    }

    /// After a waiting writer is cancelled, a *subsequent* writer must still be
    /// able to acquire (no `writer_active`/`writers_waiting` corruption).
    #[tokio::test]
    async fn cancelled_writer_lets_other_writer_proceed() {
        let gate = RwGate::new();
        let held = gate.shared().await;

        let gw = gate.clone();
        let writer = tokio::spawn(async move {
            let _e = gw.exclusive().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        writer.abort();
        let _ = writer.await;

        drop(held);
        let g = gate.clone();
        tokio::time::timeout(Duration::from_millis(500), async move {
            let _e = g.exclusive().await;
        })
        .await
        .expect("a fresh writer must acquire after a cancelled writer");
    }

    /// Cancelling a *waiting* reader must not leak `readers`. Documents the
    /// already-correct reader path (the count is only bumped on success).
    #[tokio::test]
    async fn cancelled_shared_does_not_leak_readers() {
        let gate = RwGate::new();
        let excl = gate.exclusive().await; // reader must park behind this

        let gr = gate.clone();
        let reader = tokio::spawn(async move {
            let _s = gr.shared().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        reader.abort();
        let _ = reader.await;

        {
            let inner = gate.inner.lock().unwrap();
            assert_eq!(inner.readers, 0, "cancelled reader must not leak readers");
        }

        // A fresh writer must still be able to acquire after release.
        drop(excl);
        let g = gate.clone();
        tokio::time::timeout(Duration::from_millis(500), async move {
            let _e = g.exclusive().await;
        })
        .await
        .expect("writer must acquire after a cancelled reader");
    }

    /// Two exclusive holders run strictly one at a time.
    #[tokio::test]
    async fn exclusive_serialises() {
        let gate = RwGate::new();
        let overlap = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let g = gate.clone();
            let ov = Arc::clone(&overlap);
            let mx = Arc::clone(&max);
            handles.push(tokio::spawn(async move {
                let _e = g.exclusive().await;
                let now = ov.fetch_add(1, Ordering::SeqCst) + 1;
                mx.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                ov.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "at most one exclusive holder at a time"
        );
    }
}
