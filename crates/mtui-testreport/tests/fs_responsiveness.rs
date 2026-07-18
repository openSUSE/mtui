//! Scheduler-responsiveness oracle for bead `mtui-rs-0mop.9`.
//!
//! The blocking-filesystem conversions (spawn_blocking / `tokio::fs`) exist so a
//! slow filesystem cannot wedge a Tokio worker mid-operation. On a
//! **single-threaded** runtime this is directly observable: a synchronous
//! `std::fs`-style blocking call on the worker freezes every other task, while
//! the same work moved off the worker (the pattern these conversions apply) lets
//! a concurrent heartbeat keep ticking.
//!
//! These tests pin that invariant with a `thread::sleep` standing in for a slow
//! filesystem syscall (the seam is "blocking work on the worker thread", which is
//! exactly what an `std::fs` call on a network mount is), plus a real end-to-end
//! check that the download fan-out — now writing via `spawn_blocking` — stays
//! responsive.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mtui_testreport::{BytesFetcher, ErrorMode, download_logs};
use mtui_types::Test;

/// Spawns a heartbeat task that increments `beats` every millisecond until
/// dropped. Returns the counter and the join handle's abort guard.
fn spawn_heartbeat() -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let beats = Arc::new(AtomicU64::new(0));
    let handle = {
        let beats = Arc::clone(&beats);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1)).await;
                beats.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    (beats, handle)
}

/// Baseline: a synchronous blocking call **on the worker** starves the heartbeat.
///
/// This is the behaviour the conversions remove; it documents *why* the
/// off-worker version below is required. On a single-threaded runtime the inline
/// `thread::sleep` (a slow-FS stand-in) monopolises the sole worker, so the
/// heartbeat cannot advance while it runs.
#[test]
fn inline_blocking_fs_starves_the_worker() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (beats, hb) = spawn_heartbeat();
        // Let the heartbeat prove it is alive first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = beats.load(Ordering::Relaxed);
        assert!(before > 0, "heartbeat should tick before the blocking call");

        // Block the single worker inline (simulating a slow std::fs call).
        std::thread::sleep(Duration::from_millis(100));

        let after_blocking = beats.load(Ordering::Relaxed);
        // Yield so any *pending* wakeups would land — none accrued during the
        // block because the timer could not fire on the frozen worker.
        tokio::time::sleep(Duration::from_millis(1)).await;
        hb.abort();

        // The 100ms block bought at most a couple of catch-up ticks, nowhere near
        // the ~100 a responsive worker would have accrued.
        assert!(
            after_blocking - before < 20,
            "inline block should starve the heartbeat, got {} ticks",
            after_blocking - before
        );
    });
}

/// The fix: moving the same blocking work off the worker via `spawn_blocking`
/// keeps the heartbeat ticking — the invariant the 0mop.9 conversions provide.
#[test]
fn spawn_blocking_fs_keeps_the_worker_responsive() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (beats, hb) = spawn_heartbeat();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = beats.load(Ordering::Relaxed);

        // Same 100ms of blocking work, now off the worker.
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)))
            .await
            .unwrap();

        let after = beats.load(Ordering::Relaxed);
        hb.abort();

        // The worker stayed free to run the timer, so the heartbeat kept ticking
        // across the 100ms window. A responsive worker accrues many ticks
        // (~40-100 depending on 1ms-timer granularity + scheduler overhead);
        // the threshold sits well above the starvation ceiling (<20) it must
        // beat, so the two cases never overlap while staying CI-robust.
        assert!(
            after - before > 30,
            "heartbeat should keep ticking under spawn_blocking, got {} ticks",
            after - before
        );
    });
}

/// A slow `get_bytes` used to make the download fan-out non-trivial in wall time
/// so the end-to-end responsiveness check has a window to observe.
struct SlowFetcher;

#[async_trait]
impl BytesFetcher for SlowFetcher {
    async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Ok(b"log-bytes".to_vec())
    }
}

fn test_entry(name: &str) -> Test {
    Test::new(
        name,
        "passed",
        42,
        "x86_64",
        std::collections::BTreeMap::new(),
    )
}

/// End-to-end: the real `download_logs` fan-out (which now writes each log via
/// `spawn_blocking`) does not starve a concurrent heartbeat, and still produces
/// the expected files byte-for-byte.
#[test]
fn download_fanout_stays_responsive_and_writes_correctly() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let res = dir.path().join("results");
        let inst = dir.path().join("install");
        let connectors = vec![(
            "http://h".to_string(),
            vec![test_entry("install_kernel"), test_entry("ltp")],
        )];

        let (beats, hb) = spawn_heartbeat();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let before = beats.load(Ordering::Relaxed);

        download_logs(&SlowFetcher, &connectors, &res, &inst, ErrorMode::Tolerant)
            .await
            .unwrap();

        let after = beats.load(Ordering::Relaxed);
        hb.abort();

        // Outputs unchanged (byte-for-byte) — the write path still lands the logs.
        assert_eq!(
            std::fs::read(inst.join("h-zypper-x86_64.log")).unwrap(),
            b"log-bytes"
        );
        assert!(res.join("h-x86_64-ltp.json").exists());

        // The heartbeat advanced during the fan-out (worker never wedged).
        assert!(
            after > before,
            "heartbeat must keep ticking across the download fan-out"
        );
    });
}
