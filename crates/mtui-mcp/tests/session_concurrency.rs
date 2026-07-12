//! Port of the per-template concurrency behaviours from upstream
//! `tests/test_mcp_session.py` (bead `mtui-rs-76e.11`).
//!
//! Four behaviours, matching upstream 1:1:
//!
//! * `test_run_command_unscoped_serialises_via_exclusive_gate` — unscoped calls
//!   (no real template) take the exclusive registry gate and never overlap.
//! * `test_run_command_same_rrid_serialises` — two calls scoped to the *same*
//!   template do not overlap.
//! * `test_run_command_different_rrids_run_concurrently` — two calls scoped to
//!   *different* templates overlap in time.
//! * `test_concurrent_runs_do_not_clobber_each_others_stdout` — overlapping
//!   different-RRID runs each capture only their own output.
//!
//! The first two assert the **lock discipline** landed by `mtui-rs-76e.11` and
//! pass now. The last two assert **genuine wall-clock concurrency / per-call
//! output isolation**, which additionally needs the `mtui-core` change that stops
//! dispatch taking `&mut Session` for the whole monolithic session (and isolates
//! per-call output) — tracked as `mtui-rs-f36r`. They are `#[ignore]`d here so
//! the parity target is captured, not dropped; un-ignore them in `mtui-rs-f36r`.

#![cfg(feature = "mcp")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::ArgMatches;
use mtui_config::Config;
use mtui_core::{Command, CommandResult, Registry, Scope, Session};
use mtui_mcp::McpSession;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;

const RRID_A: &str = "SUSE:Maintenance:1:1";
const RRID_B: &str = "SUSE:Maintenance:2:1";

/// A shared, ordered record of `(rrid, start, end)` intervals a probe command
/// appends to, so a test can check overlap / serialisation across calls.
type Intervals = Arc<Mutex<Vec<(String, Instant, Instant)>>>;

/// A test-only command that sleeps briefly and records its acting template's
/// RRID plus its `[start, end]` interval, so concurrent invocations reveal
/// whether they overlapped.
struct Probe {
    name: &'static str,
    scope: Scope,
    hold: Duration,
    seen: Intervals,
    /// Also print the RRID's lines around the sleep, for the stdout-clobber test.
    print_output: bool,
}

#[async_trait::async_trait]
impl Command for Probe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn scope(&self) -> Scope {
        self.scope
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = session.metadata().id();
        if self.print_output {
            session.display.println(&format!("{rrid}:first"));
        }
        // Blocking sleep is fine: the point is to hold whatever lock the caller
        // acquired for a measurable window.
        let start = Instant::now();
        std::thread::sleep(self.hold);
        let end = Instant::now();
        if self.print_output {
            session.display.println(&format!("{rrid}:second"));
        }
        self.seen.lock().unwrap().push((rrid, start, end));
        Ok(())
    }
}

/// Build a registry containing a single [`Probe`] command.
fn registry_with(probe: Probe) -> Registry {
    let mut reg = Registry::new();
    reg.register(Arc::new(probe));
    reg
}

/// A session over a throwaway temp `template_dir`.
fn session() -> Arc<McpSession> {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    // Leak the tempdir guard: the session outlives this fn and only reads the
    // path; the OS reclaims it at process exit.
    std::mem::forget(tmp);
    McpSession::new(config)
}

/// Load two active-able reports (`RRID_A`, `RRID_B`) into the session, `A` active.
async fn load_two(session: &McpSession) {
    let mut guard = session.session().lock().await;
    for rrid in [RRID_A, RRID_B] {
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        guard.templates.add(Box::new(report));
    }
    guard.templates.set_active(RRID_A);
}

/// Sorted intervals; assert strict non-overlap (each ends at-or-before the next
/// starts).
fn assert_serial(seen: &Intervals) {
    let mut ivals: Vec<(Instant, Instant)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s, e)| (*s, *e))
        .collect();
    ivals.sort();
    for w in ivals.windows(2) {
        let (_a_start, a_end) = w[0];
        let (b_start, _b_end) = w[1];
        assert!(a_end <= b_start, "intervals overlapped: {ivals:?}");
    }
}

/// Unscoped commands (no real template loaded) take the exclusive registry gate,
/// so concurrent calls run strictly one-at-a-time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unscoped_serialises_via_exclusive_gate() {
    let seen: Intervals = Arc::new(Mutex::new(Vec::new()));
    let reg = registry_with(Probe {
        name: "probe_unscoped",
        scope: Scope::Fanout,
        hold: Duration::from_millis(50),
        seen: Arc::clone(&seen),
        print_output: false,
    });
    let sess = session(); // nothing loaded → resolves to the null report only

    let r = &reg;
    let s = sess.as_ref();
    tokio::join!(
        async { s.run_command(r, "probe_unscoped", &[]).await.unwrap() },
        async { s.run_command(r, "probe_unscoped", &[]).await.unwrap() },
        async { s.run_command(r, "probe_unscoped", &[]).await.unwrap() },
    );

    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_serial(&seen);
}

/// Two calls scoped to the *same* template serialise (share one per-RRID lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_rrid_serialises() {
    let seen: Intervals = Arc::new(Mutex::new(Vec::new()));
    let reg = registry_with(Probe {
        name: "probe_scoped",
        scope: Scope::Fanout,
        hold: Duration::from_millis(80),
        seen: Arc::clone(&seen),
        print_output: false,
    });
    let sess = session();
    load_two(&sess).await;

    let r = &reg;
    let s = sess.as_ref();
    let a1 = vec!["-T".to_owned(), RRID_A.to_owned()];
    let a2 = a1.clone();
    tokio::join!(
        async { s.run_command(r, "probe_scoped", &a1).await.unwrap() },
        async { s.run_command(r, "probe_scoped", &a2).await.unwrap() },
    );

    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_serial(&seen);
}

/// Two calls scoped to *different* templates overlap in time.
///
/// Ignored until `mtui-rs-f36r`: they take distinct per-RRID locks (as they
/// must), but the inner `Mutex<Session>` dispatch still serialises them, so no
/// wall-clock overlap is observable yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "genuine concurrency needs mtui-core dispatch change: bead mtui-rs-f36r"]
async fn different_rrids_run_concurrently() {
    let seen: Intervals = Arc::new(Mutex::new(Vec::new()));
    let reg = registry_with(Probe {
        name: "probe_scoped",
        scope: Scope::Fanout,
        hold: Duration::from_millis(100),
        seen: Arc::clone(&seen),
        print_output: false,
    });
    let sess = session();
    load_two(&sess).await;

    let r = &reg;
    let s = sess.as_ref();
    let a = vec!["-T".to_owned(), RRID_A.to_owned()];
    let b = vec!["-T".to_owned(), RRID_B.to_owned()];
    tokio::join!(
        async { s.run_command(r, "probe_scoped", &a).await.unwrap() },
        async { s.run_command(r, "probe_scoped", &b).await.unwrap() },
    );

    let ivals = seen.lock().unwrap();
    assert_eq!(ivals.len(), 2);
    let (_r1, s1, e1) = &ivals[0];
    let (_r2, s2, e2) = &ivals[1];
    // Each holds for 100ms; a serial run would not overlap.
    assert!(s2 < e1, "expected overlap, got {ivals:?}");
    assert!(s1 < e2, "expected overlap, got {ivals:?}");
}

/// Overlapping different-RRID runs each capture only their own output.
///
/// Ignored until `mtui-rs-f36r`: per-call output isolation needs the per-call
/// display (task-local) that lands with the core dispatch change; today both
/// calls share the one `Session.display`/`SharedBuf`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "per-call output isolation needs mtui-core change: bead mtui-rs-f36r"]
async fn do_not_clobber_each_others_stdout() {
    let seen: Intervals = Arc::new(Mutex::new(Vec::new()));
    let reg = registry_with(Probe {
        name: "probe_out",
        scope: Scope::Fanout,
        hold: Duration::from_millis(100),
        seen: Arc::clone(&seen),
        print_output: true,
    });
    let sess = session();
    load_two(&sess).await;

    let r = &reg;
    let s = sess.as_ref();
    let a = vec!["-T".to_owned(), RRID_A.to_owned()];
    let b = vec!["-T".to_owned(), RRID_B.to_owned()];
    let (out_a, out_b) = tokio::join!(
        async { s.run_command(r, "probe_out", &a).await.unwrap() },
        async { s.run_command(r, "probe_out", &b).await.unwrap() },
    );

    assert_eq!(out_a, format!("{RRID_A}:first\n{RRID_A}:second\n"));
    assert_eq!(out_b, format!("{RRID_B}:first\n{RRID_B}:second\n"));
}
