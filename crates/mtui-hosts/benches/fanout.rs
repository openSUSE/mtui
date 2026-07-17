//! Perf baselines for host fan-out and SFTP/lock I/O (mtui-rs-0mop.1).
//!
//! Measurement-only: these record the *current* behaviour of
//! [`HostsGroup`]'s parallel fan-out so the remediation beads have a baseline to
//! diff against. Everything runs offline against [`MockConnection`] — no real
//! sshd, no network — per the workspace testing rules.
//!
//! What each group measures and which child bead it feeds:
//! - `fanout/run` at fleet sizes {1,4,16,64,256} with a fixed per-host delay:
//!   the scaling curve of the current **unbounded** `join_all` parallel batch.
//!   A flat curve until memory/scheduler pressure is the baseline for bounding
//!   (0mop.2) and session reuse (0mop.3).
//! - `sftp/put` / `sftp/get`: per-host SFTP fan-out cost (0mop.6).
//! - `locks/report`: `report_locks` re-reads each host's lock file over SSH; the
//!   per-host read count is the baseline for eliminating repeated reads (0mop.4).
//!
//! Wall-clock here is advisory (async scheduling is noisy); the deterministic
//! regression oracle is the per-host Connection call count, asserted in the
//! integration tests, not timed here.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::hostlog::CommandLog;

/// Fleet sizes swept across every fan-out group.
const FLEET_SIZES: &[usize] = &[1, 4, 16, 64, 256];

/// A small fixed per-host command latency so the bench measures *scaling shape*
/// (does N hosts stay roughly flat, or grow with N?) rather than pure CPU. Kept
/// tiny so the largest fleet stays sub-second.
const PER_HOST_DELAY: Duration = Duration::from_micros(200);

/// Builds an N-host [`HostsGroup`], every host enabled + parallel, backed by a
/// [`MockConnection`] that returns a canned success after `PER_HOST_DELAY`.
fn build_group(n: usize, delay: Duration) -> HostsGroup {
    let targets: Vec<Target> = (0..n)
        .map(|i| {
            let host = format!("host-{i:04}");
            let conn = MockConnection::new(&host)
                .with_default(CommandLog::new("", "ok", "", 0, 1))
                .with_run_delay(delay);
            Target::with_connection(
                host,
                TargetState::Enabled,
                ExecutionMode::Parallel,
                Box::new(conn),
            )
        })
        .collect();
    HostsGroup::new(targets, false)
}

/// A single multi-threaded runtime reused across iterations so runtime spin-up
/// is not folded into the measured time.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn bench_fanout_run(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("fanout/run");
    for &n in FLEET_SIZES {
        g.throughput(criterion::Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || build_group(n, PER_HOST_DELAY),
                |mut group| async move {
                    group.run(black_box("true")).await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

fn bench_sftp(c: &mut Criterion) {
    let rt = rt();
    let local = Path::new("/tmp/mtui-bench-payload");
    let remote = Path::new("/tmp/mtui-bench-remote");

    let mut put = c.benchmark_group("sftp/put");
    for &n in FLEET_SIZES {
        put.throughput(criterion::Throughput::Elements(n as u64));
        put.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || build_group(n, Duration::ZERO),
                |mut group| async move {
                    group.sftp_put(black_box(local), black_box(remote)).await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    put.finish();

    let mut get = c.benchmark_group("sftp/get");
    for &n in FLEET_SIZES {
        get.throughput(criterion::Throughput::Elements(n as u64));
        get.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || build_group(n, Duration::ZERO),
                |mut group| async move {
                    group
                        .sftp_get(black_box("remote.log"), black_box(local))
                        .await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    get.finish();
}

fn bench_report_locks(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("locks/report");
    for &n in FLEET_SIZES {
        g.throughput(criterion::Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || build_group(n, PER_HOST_DELAY),
                |mut group| async move {
                    group
                        .report_locks(|_host, _sys, _row| { /* drain */ }, false)
                        .await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

criterion_group!(benches, bench_fanout_run, bench_sftp, bench_report_locks);
criterion_main!(benches);
