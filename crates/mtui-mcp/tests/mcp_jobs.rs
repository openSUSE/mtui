//! Port of the background-job (async slow-op) path from upstream
//! `tests/test_mcp_jobs.py` (bead `mtui-rs-76e.12`).
//!
//! A backgrounded command runs in a spawned worker that goes through the same
//! [`McpSession::run_command`] primitive (so it takes the same per-RRID /
//! registry gate and output cap as a foreground call) and records its outcome on
//! the job table; `job_status` / `job_result` read that table. These tests drive
//! the lifecycle with the real `whoami` command (fast, no hosts) and a couple of
//! test-only probe commands, matching the style of `session_concurrency.rs`.

#![cfg(feature = "mcp")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clap::ArgMatches;
use mtui_config::Config;
use mtui_core::{Command, CommandResult, Registry, Scope, Session, register_all};
use mtui_mcp::{JobState, McpSession};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use tokio::sync::Notify;

const RRID_A: &str = "SUSE:Maintenance:1:1";
const RRID_B: &str = "SUSE:Maintenance:2:1";

/// A session over a throwaway temp `template_dir`, with `session_user` set so
/// `whoami` produces a deterministic banner.
fn session() -> Arc<McpSession> {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.session_user = "testuser".to_owned();
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

/// Await a job's terminal state by polling its status (the worker records the
/// outcome asynchronously). Fails the test if it does not settle promptly.
async fn await_terminal(session: &McpSession, job_id: &str) -> JobState {
    for _ in 0..500 {
        let state = session.job_status(job_id).expect("job exists").state;
        if state != JobState::Running {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("job {job_id} did not reach a terminal state");
}

/// A test-only fan-out command that prints its acting template's RRID.
struct FanoutProbe;

#[async_trait::async_trait]
impl Command for FanoutProbe {
    fn name(&self) -> &'static str {
        "fanout_job_probe"
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = session.metadata().id();
        session.display.println(&rrid);
        Ok(())
    }
}

/// A registry with the full command set plus one extra probe.
fn registry_with_probe(probe: Arc<dyn Command>) -> Arc<Registry> {
    let mut reg = register_all();
    reg.register(probe);
    Arc::new(reg)
}

// --------------------------------------------------------------------------- //
// Single-job lifecycle                                                        //
// --------------------------------------------------------------------------- //

/// A backgrounded command finishes `done` and yields its stdout.
#[tokio::test]
async fn start_job_runs_and_result_returns_stdout() {
    let sess = session();
    let registry = Arc::new(register_all());

    let job_id = sess
        .start_job(Arc::clone(&registry), "whoami", Vec::new())
        .expect("start_job succeeds");
    assert!(job_id.starts_with("whoami-"), "id shape: {job_id}");

    let state = await_terminal(&sess, &job_id).await;
    assert_eq!(state, JobState::Done);

    let status = sess.job_status(&job_id).expect("status");
    assert_eq!(status.command, "whoami");

    let result = sess.job_result(&job_id).expect("done job yields stdout");
    assert!(
        result.starts_with("User: testuser, app pid: "),
        "got: {result:?}"
    );
}

/// A job whose command fails records `failed`; `job_result` raises.
#[tokio::test]
async fn job_result_failed_surfaces_error_envelope() {
    let sess = session();
    let registry = Arc::new(register_all());

    let job_id = sess
        .start_job(
            Arc::clone(&registry),
            "whoami",
            vec!["--nonexistent-flag".to_owned()],
        )
        .expect("start_job succeeds");
    let state = await_terminal(&sess, &job_id).await;
    assert_eq!(state, JobState::Failed);

    let err = sess.job_result(&job_id).expect_err("failed job raises");
    // A parse failure is argparse-exit-2.
    assert_eq!(err.exit_code, 2, "got: {err:?}");
}

/// `job_result` on a still-running job raises, pointing at `job_status`.
#[tokio::test]
async fn job_result_running_tells_caller_to_poll() {
    let sess = session();
    // A probe that blocks until released, so the job stays running.
    let gate = Arc::new(Notify::new());
    let blocker = Blocker {
        release: Arc::clone(&gate),
        started: Arc::new(Notify::new()),
    };
    let started = Arc::clone(&blocker.started);
    let registry = registry_with_probe(Arc::new(blocker));

    let job_id = sess
        .start_job(Arc::clone(&registry), "blocking_job_probe", Vec::new())
        .expect("start_job succeeds");
    // Wait until the body is actually executing.
    started.notified().await;

    let err = sess
        .job_result(&job_id)
        .expect_err("running job raises on job_result");
    assert!(err.stderr.contains("still running"), "got: {err:?}");
    assert!(err.stderr.contains("poll job_status"), "got: {err:?}");

    // Release the body and let it settle so the worker does not outlive the test.
    gate.notify_one();
    await_terminal(&sess, &job_id).await;
}

/// Querying an unknown job id raises a clean error.
#[test]
fn job_status_unknown_id_raises() {
    let sess = session();
    let err = sess.job_status("nope-1").expect_err("unknown id raises");
    assert!(err.stderr.contains("no such job"), "got: {err:?}");
}

/// `job_list` enumerates every started job with its state.
#[tokio::test]
async fn job_list_reports_started_jobs() {
    let sess = session();
    let registry = Arc::new(register_all());

    let a = sess
        .start_job(Arc::clone(&registry), "whoami", Vec::new())
        .expect("start_job a");
    let b = sess
        .start_job(Arc::clone(&registry), "whoami", Vec::new())
        .expect("start_job b");
    await_terminal(&sess, &a).await;
    await_terminal(&sess, &b).await;

    let jobs = sess.job_list();
    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter().all(|j| j.state == JobState::Done),
        "both done: {jobs:?}"
    );
}

/// Cancelling an unknown job id raises a clean error.
#[tokio::test]
async fn job_cancel_unknown_id_raises() {
    let sess = session();
    let err = sess
        .job_cancel("nope-1")
        .await
        .expect_err("unknown id raises");
    assert!(err.stderr.contains("no such job"), "got: {err:?}");
}

// --------------------------------------------------------------------------- //
// Per-template fan-out                                                         //
// --------------------------------------------------------------------------- //

/// With no fan-out, `start_jobs` mints one job with the legacy id shape.
#[tokio::test]
async fn start_jobs_single_template_keeps_one_job() {
    let sess = session();
    let registry = Arc::new(register_all());

    let ids = sess
        .start_jobs(Arc::clone(&registry), "whoami", Vec::new())
        .await
        .expect("start_jobs succeeds");
    assert_eq!(ids.len(), 1);
    assert!(ids[0].starts_with("whoami-"), "id shape: {}", ids[0]);
    assert!(
        !ids[0].contains("SUSE"),
        "no RRID in single-job id: {}",
        ids[0]
    );
    await_terminal(&sess, &ids[0]).await;
}

/// A fanned-out slow command mints one job per loaded template.
#[tokio::test]
async fn start_jobs_fans_out_one_job_per_template() {
    let sess = session();
    load_two(&sess).await;
    let registry = registry_with_probe(Arc::new(FanoutProbe));

    let ids = sess
        .start_jobs(Arc::clone(&registry), "fanout_job_probe", Vec::new())
        .await
        .expect("start_jobs succeeds");
    assert_eq!(ids.len(), 2);
    // ids encode the (sanitised) RRID and are unique.
    assert!(ids.iter().any(|j| j.contains("SUSE_Maintenance_1_1")));
    assert!(ids.iter().any(|j| j.contains("SUSE_Maintenance_2_1")));
    assert_ne!(ids[0], ids[1]);

    for id in &ids {
        await_terminal(&sess, id).await;
    }
    let listed = sess.job_list();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|j| j.state == JobState::Done));

    let mut outputs: Vec<String> = ids
        .iter()
        .map(|id| sess.job_result(id).expect("done").trim().to_owned())
        .collect();
    outputs.sort();
    assert_eq!(outputs, vec![RRID_A.to_owned(), RRID_B.to_owned()]);
}

/// A client-supplied `-T` narrows to one template → one job.
#[tokio::test]
async fn start_jobs_explicit_template_yields_single_job() {
    let sess = session();
    load_two(&sess).await;
    let registry = registry_with_probe(Arc::new(FanoutProbe));

    let ids = sess
        .start_jobs(
            Arc::clone(&registry),
            "fanout_job_probe",
            vec!["-T".to_owned(), RRID_B.to_owned()],
        )
        .await
        .expect("start_jobs succeeds");
    assert_eq!(ids.len(), 1);
    await_terminal(&sess, &ids[0]).await;
    assert_eq!(sess.job_result(&ids[0]).expect("done").trim(), RRID_B);
}

/// Cancelling one per-template job does not abort the sibling jobs.
#[tokio::test]
async fn cancel_one_template_job_leaves_others() {
    let sess = session();
    load_two(&sess).await;

    // The first template's body blocks on a per-RRID notify; the second returns
    // at once. We cancel the first and assert the second still completes.
    let probe = PerRridBlocker {
        blocking_rrid: RRID_A.to_owned(),
        release: Arc::new(Notify::new()),
        started: Arc::new(Notify::new()),
        started_count: Arc::new(AtomicUsize::new(0)),
    };
    let release = Arc::clone(&probe.release);
    let started = Arc::clone(&probe.started);
    let registry = registry_with_probe(Arc::new(probe));

    let ids = sess
        .start_jobs(Arc::clone(&registry), "per_rrid_blocking_probe", Vec::new())
        .await
        .expect("start_jobs succeeds");
    assert_eq!(ids.len(), 2);
    let first = ids
        .iter()
        .find(|j| j.contains("SUSE_Maintenance_1_1"))
        .expect("job for A")
        .clone();
    let second = ids
        .iter()
        .find(|j| j.contains("SUSE_Maintenance_2_1"))
        .expect("job for B")
        .clone();

    // Wait until the blocking body is running, then cancel it and release.
    started.notified().await;
    let msg = sess.job_cancel(&first).await.expect("cancel succeeds");
    assert_eq!(msg, format!("cancelled job {first}"));
    release.notify_waiters();

    assert_eq!(
        sess.job_status(&first).expect("first exists").state,
        JobState::Cancelled
    );
    let second_state = await_terminal(&sess, &second).await;
    assert_eq!(second_state, JobState::Done);
    assert_eq!(sess.job_result(&second).expect("done").trim(), RRID_B);
}

// --------------------------------------------------------------------------- //
// Resource limits (bead mtui-rs-th4o.8)                                        //
// --------------------------------------------------------------------------- //

/// A session with explicit active/completed job caps and a deterministic user.
fn session_with_caps(max_active: usize, max_completed: usize) -> Arc<McpSession> {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.session_user = "testuser".to_owned();
    config.mcp_max_active_jobs = max_active;
    config.mcp_max_completed_jobs = max_completed;
    std::mem::forget(tmp);
    McpSession::new(config)
}

/// Spawning up to the active cap succeeds; the next spawn is rejected before a
/// worker is created, and freeing a slot re-enables spawning.
#[tokio::test]
async fn active_job_cap_rejects_before_spawn_and_frees_on_finish() {
    let sess = session_with_caps(2, 0);
    // Two blocking probes occupy both active slots. Both records are inserted as
    // Running synchronously by `start_job` (before either body runs), so the cap
    // sees two active jobs regardless of the registry-gate serialisation.
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let registry = registry_with_probe(Arc::new(FlagBlocker {
        release: Arc::clone(&flag),
    }));

    let a = sess
        .start_job(Arc::clone(&registry), "flag_blocking_probe", Vec::new())
        .expect("first fits");
    let b = sess
        .start_job(Arc::clone(&registry), "flag_blocking_probe", Vec::new())
        .expect("second fits");

    // The third is rejected without spawning.
    let err = sess
        .start_job(Arc::clone(&registry), "flag_blocking_probe", Vec::new())
        .expect_err("third exceeds the cap");
    assert_eq!(err.exit_code, 1);
    assert!(err.stderr.contains("too many active jobs"), "got: {err:?}");
    // Only the two admitted jobs exist — the rejected one never entered the table.
    assert_eq!(sess.job_list().len(), 2);

    // Release both; once they settle a slot frees for a new spawn.
    flag.store(true, Ordering::SeqCst);
    await_terminal(&sess, &a).await;
    await_terminal(&sess, &b).await;
    let c = sess
        .start_job(Arc::clone(&registry), "whoami", Vec::new())
        .expect("slot freed after finish");
    await_terminal(&sess, &c).await;
}

/// A fan-out that would breach the active cap is rejected as a whole (no partial
/// spawn).
#[tokio::test]
async fn fanout_breaching_active_cap_is_rejected_whole() {
    let sess = session_with_caps(1, 0);
    load_two(&sess).await;
    let registry = registry_with_probe(Arc::new(FanoutProbe));

    // Two templates resolve → 2 jobs, but the cap is 1.
    let err = sess
        .start_jobs(Arc::clone(&registry), "fanout_job_probe", Vec::new())
        .await
        .expect_err("fan-out exceeds the cap");
    assert!(err.stderr.contains("too many active jobs"), "got: {err:?}");
    // Nothing was spawned — the batch is atomic.
    assert_eq!(sess.job_list().len(), 0);
}

/// A zero active cap disables the limit (any number of jobs may run).
#[tokio::test]
async fn zero_active_cap_disables_the_limit() {
    let sess = session_with_caps(0, 0);
    let registry = Arc::new(register_all());
    let mut ids = Vec::new();
    for _ in 0..8 {
        ids.push(
            sess.start_job(Arc::clone(&registry), "whoami", Vec::new())
                .expect("no cap → always admitted"),
        );
    }
    for id in &ids {
        await_terminal(&sess, id).await;
    }
    assert_eq!(sess.job_list().len(), 8);
}

/// Completed records are FIFO-evicted to the completed cap; running jobs survive
/// and evicted ids report cleanly as unknown.
#[tokio::test]
async fn completed_jobs_are_fifo_evicted_to_the_cap() {
    let sess = session_with_caps(0, 2);
    let registry = Arc::new(register_all());

    // Run five fast jobs to completion, in order, so `finished` is monotonic.
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = sess
            .start_job(Arc::clone(&registry), "whoami", Vec::new())
            .expect("admitted");
        await_terminal(&sess, &id).await;
        ids.push(id);
    }

    // Only the newest two terminal records survive.
    let listed = sess.job_list();
    assert_eq!(listed.len(), 2, "capped to newest two: {listed:?}");
    let survivors: std::collections::HashSet<_> = listed.iter().map(|j| j.id.clone()).collect();
    assert!(survivors.contains(&ids[3]), "second-newest kept");
    assert!(survivors.contains(&ids[4]), "newest kept");

    // The three oldest were evicted and now report cleanly as unknown.
    for old in &ids[..3] {
        let err = sess.job_status(old).expect_err("evicted id is unknown");
        assert!(err.stderr.contains("no such job"), "got: {err:?}");
    }
}

// The "eviction spares running jobs" invariant is proven deterministically as a
// unit test in `session.rs` (it needs direct access to the private jobs table;
// an integration test cannot drive a concurrent completion while another job
// holds the single session mutex — see the `mtui-rs-f36r` caveat).

// --------------------------------------------------------------------------- //
// Test-only blocking probes                                                   //
// --------------------------------------------------------------------------- //

/// A self-scoped probe whose body blocks until `release` is notified.
struct Blocker {
    release: Arc<Notify>,
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl Command for Blocker {
    fn name(&self) -> &'static str {
        "blocking_job_probe"
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// A self-scoped probe that blocks by *polling* an `AtomicBool` until it is set.
///
/// Unlike [`Blocker`] (a `Notify`, which only wakes tasks already parked at
/// `.notified()`), this survives wake races and workers that arrive at the block
/// late — the right primitive for the resource-limit tests, where several
/// same-scope jobs serialise on the registry gate and reach the block at
/// different times.
struct FlagBlocker {
    release: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl Command for FlagBlocker {
    fn name(&self) -> &'static str {
        "flag_blocking_probe"
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        while !self.release.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(())
    }
}

/// A fan-out probe whose body blocks only for `blocking_rrid`; other templates
/// return at once (their body just prints the RRID).
struct PerRridBlocker {
    blocking_rrid: String,
    release: Arc<Notify>,
    started: Arc<Notify>,
    started_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Command for PerRridBlocker {
    fn name(&self) -> &'static str {
        "per_rrid_blocking_probe"
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = session.metadata().id();
        if rrid == self.blocking_rrid {
            // Signal once, on the first entry to the blocking body.
            if self.started_count.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
            }
            self.release.notified().await;
        } else {
            session.display.println(&rrid);
        }
        Ok(())
    }
}
