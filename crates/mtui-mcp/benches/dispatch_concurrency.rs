//! Contention measurement for different-RRID MCP dispatch (mtui-rs-0mop.11).
//!
//! Measurement-only, offline; requires the `mcp` feature. The lock *discipline*
//! (RwGate + per-RRID locks) is already correct, but `McpSession::run_command`
//! holds one monolithic `Arc<Mutex<Session>>` over the whole dispatch, so N
//! commands scoped to N *distinct* RRIDs still serialise on that inner mutex.
//!
//! This bench dispatches N probe commands — each scoped to its own loaded RRID
//! and each holding a fixed CPU-free `hold` window inside the session lock — via
//! `join_all`, at N ∈ {1,2,4,8}. The verdict oracle is the *shape* of the curve:
//!
//! * **Serialised** (today): wall-clock ≈ N × hold (each waits for the mutex).
//! * **Concurrent** (post-f36r refactor): wall-clock ≈ ~1 × hold (they overlap).
//!
//! Wall-clock async timing is scheduler-noisy (the baseline calls it advisory);
//! the deterministic oracle for the eventual fix is the interval-overlap
//! assertion in `tests/session_concurrency.rs`, not these numbers. This bench
//! only quantifies *whether* the contention is material enough to justify the
//! Phase-2 refactor (see plans/mtui-rs-0mop.11-mcp-lock-contention.md).

use std::sync::Arc;
use std::time::Duration;

use clap::ArgMatches;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_config::Config;
use mtui_core::{Command, CommandResult, Registry, Scope, Session};
use mtui_mcp::McpSession;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;

/// A test-only command that holds the dispatch lock for a fixed window.
///
/// Sleeps `hold` synchronously (the point is to occupy whatever lock the caller
/// took for a measurable, deterministic window), so N concurrent invocations
/// reveal whether they overlapped.
struct Hold {
    hold: Duration,
}

#[async_trait::async_trait]
impl Command for Hold {
    fn name(&self) -> &'static str {
        "hold"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        // Blocking sleep: occupies the held session mutex for a fixed window.
        std::thread::sleep(self.hold);
        Ok(())
    }
}

fn registry(hold: Duration) -> Arc<Registry> {
    let mut reg = Registry::new();
    reg.register(Arc::new(Hold { hold }));
    Arc::new(reg)
}

/// A session with `n` loaded reports (`SUSE:Maintenance:<i>:1`), the first active.
async fn session_with(n: usize) -> (Arc<McpSession>, Vec<String>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.session_user = "benchuser".to_owned();
    std::mem::forget(tmp);
    let sess = McpSession::new(config);

    let mut rrids = Vec::with_capacity(n);
    {
        let mut guard = sess.session().lock().await;
        for i in 1..=n {
            let rrid = format!("SUSE:Maintenance:{i}:1");
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse(&rrid).unwrap());
            guard.templates.add(Box::new(report));
            rrids.push(rrid);
        }
        if let Some(first) = rrids.first() {
            guard.templates.set_active(first);
        }
    }
    (sess, rrids)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

/// Dispatch one `hold`-scoped call per RRID concurrently and wait for all.
async fn fan(session: &McpSession, registry: &Registry, rrids: &[String]) {
    let futs = rrids.iter().map(|rrid| {
        let argv = vec!["-T".to_owned(), rrid.clone()];
        async move { session.run_command(registry, "hold", &argv).await.unwrap() }
    });
    futures::future::join_all(futs).await;
}

fn bench_dispatch_concurrency(c: &mut Criterion) {
    let rt = rt();
    // A hold long enough to dwarf the ~257ns lock + mock command overhead, so the
    // serial-vs-parallel difference is the dispatch-mutex window, not noise.
    let hold = Duration::from_millis(5);
    let registry = registry(hold);

    let mut g = c.benchmark_group("mcp/dispatch_concurrency");
    // Wall-clock is dominated by the fixed hold; a handful of samples suffices to
    // read the serial (N×hold) vs concurrent (~1×hold) shape.
    g.sample_size(10);
    for n in [1usize, 2, 4, 8] {
        let (session, rrids) = rt.block_on(session_with(n));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.to_async(&rt)
                .iter(|| async { fan(session.as_ref(), registry.as_ref(), &rrids).await });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_dispatch_concurrency);
criterion_main!(benches);
