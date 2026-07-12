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
//! ## Interim locking depth (bead `mtui-rs-76e.11` / follow-up `mtui-rs-f36r`)
//!
//! This gate + the per-RRID lock map in [`crate::session`] land the correct lock
//! *discipline*: same-RRID and unscoped calls serialise, and registry mutators
//! drain in-flight per-RRID work. Genuine wall-clock concurrency between
//! *different-RRID* calls additionally needs `mtui-core` to stop taking
//! `&mut Session` for the whole monolithic session on dispatch (and to isolate
//! per-call output); that core change is tracked as `mtui-rs-f36r`. Until then
//! two different-RRID calls acquire distinct per-RRID locks (as they must) but
//! still serialise on the inner session mutex during dispatch.

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
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires the gate in shared (reader) mode, waiting out any active or
    /// pending exclusive holder (writer preference).
    ///
    /// The returned [`SharedGuard`] releases the hold on drop.
    pub async fn shared(&self) -> SharedGuard {
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
    pub async fn exclusive(&self) -> ExclusiveGuard {
        {
            let mut inner = self.inner.lock().expect("rw gate poisoned");
            inner.writers_waiting += 1;
        }
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = self.inner.lock().expect("rw gate poisoned");
                if inner.readers == 0 && !inner.writer_active {
                    inner.writer_active = true;
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
