//! Perf baseline for MCP session lock behaviour (mtui-rs-0mop.1).
//!
//! Measurement-only, offline; requires the `mcp` feature (the session model is
//! gated behind it). Measures [`McpSession::scoped_lock`] acquire+release, the
//! per-dispatch gate the MCP command path takes on every tool call:
//! - `mcp/scoped_lock/uncontended`: distinct RRID keys — the fast path.
//! - `mcp/scoped_lock/contended`: one shared RRID key acquired/released in a
//!   tight loop — the baseline for reducing session lock contention (0mop.11).
//!
//! Full tool-dispatch and background-job throughput are measured as request /
//! job counts in the integration tests rather than timed here (they are
//! dominated by the mocked command work); see plans/perf-baseline-0mop1.md.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use mtui_config::Config;
use mtui_mcp::session::McpSession;

fn session() -> Arc<McpSession> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.session_user = "benchuser".to_owned();
    std::mem::forget(tmp);
    McpSession::new(config)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn bench_scoped_lock(c: &mut Criterion) {
    let rt = rt();
    let session = session();

    let mut g = c.benchmark_group("mcp/scoped_lock");
    g.bench_function("uncontended", |b| {
        let mut i = 0u64;
        b.to_async(&rt).iter(|| {
            i += 1;
            let session = session.clone();
            async move {
                // A unique key each iteration: no cross-iteration contention.
                let key = format!("S:M:{i}:1");
                let lock = session.scoped_lock(black_box(Some(&key))).await;
                drop(lock);
            }
        });
    });
    g.bench_function("contended", |b| {
        b.to_async(&rt).iter(|| {
            let session = session.clone();
            async move {
                // Same key every iteration: exercises the keyed-lock hot path.
                let lock = session.scoped_lock(black_box(Some("S:M:1:1"))).await;
                drop(lock);
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_scoped_lock);
criterion_main!(benches);
