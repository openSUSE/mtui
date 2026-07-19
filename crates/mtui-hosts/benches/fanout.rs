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
//! - `sftp/put` / `sftp/get`: per-host SFTP fan-out cost (0mop.6). `put` uses a
//!   real size-bounded payload so it exercises the read-once shared-buffer path
//!   (one disk read, shared bytes to every host) rather than per-host re-reads.
//! - `history/append`: cost of recording one history entry vs. the size the log
//!   already holds. Flat = the append primitive (0mop.5); a rising curve would
//!   be the old read-rewrite emulation.
//! - `locks/report`: `report_locks` re-reads each host's lock file over SSH; the
//!   per-host read count is the baseline for eliminating repeated reads (0mop.4).
//! - `discovery/parse_system` vs `discovery/per_op_reads`: on a high-latency
//!   host with K product files, the batched single-session `parse_system`
//!   (0mop.3) pays one SFTP handshake regardless of K, while the per-op read
//!   path pays one per read — the batched curve stays flat in K, the per-op
//!   curve grows linearly.
//!
//! Wall-clock here is advisory (async scheduling is noisy); the deterministic
//! regression oracle is the per-host Connection call count, asserted in the
//! integration tests, not timed here.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_hosts::connection::Connection;
use mtui_hosts::{HostsGroup, MockConnection, Target, parse_system};
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

/// The bounded counterpart to `fanout/run` (0mop.2). Sweeps the same fleet
/// sizes with a fixed `max_parallel` cap so the "after" curve can be diffed
/// against the unbounded baseline: for fleets at/below the cap the two must stay
/// ~flat and equivalent; above the cap the bounded curve trades a small latency
/// increase for a hard ceiling on peak concurrency (sockets/tasks/RSS).
fn bench_fanout_run_bounded(c: &mut Criterion) {
    const BOUND: usize = 16;
    let rt = rt();
    let mut g = c.benchmark_group("fanout/run_bounded");
    for &n in FLEET_SIZES {
        g.throughput(criterion::Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let mut group = build_group(n, PER_HOST_DELAY);
                    group.set_max_parallel(BOUND);
                    group
                },
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
    // A real, size-bounded payload so `sftp_put` exercises the read-once shared
    // path (0mop.6): one disk read, shared `Arc<[u8]>` fanned to every host.
    let payload = tempfile::NamedTempFile::new().expect("temp payload");
    std::fs::write(payload.path(), vec![0u8; 4096]).expect("write payload");
    let local = payload.path();
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

/// `locks/lock` (whole-group) vs. `locks/lock_scoped` (a `-t` subset): the
/// operation-lock fan-out cost, and the baseline for the scoped variant added
/// with the `run` lock-enforcement fix (`mtui-rs-bwu2`). Scoping to a subset
/// must never be slower than the whole-group lock over the same fleet — it only
/// narrows the `run_fanout` predicate. Wall-clock is advisory (see module docs);
/// the deterministic oracle is the per-host `Connection` call count in the unit
/// tests, not timed here.
fn bench_lock(c: &mut Criterion) {
    let rt = rt();

    let mut whole = c.benchmark_group("locks/lock");
    for &n in FLEET_SIZES {
        whole.throughput(criterion::Throughput::Elements(n as u64));
        whole.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || build_group(n, PER_HOST_DELAY),
                |mut group| async move {
                    group.lock(black_box("")).await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    whole.finish();

    let mut scoped = c.benchmark_group("locks/lock_scoped");
    for &n in FLEET_SIZES {
        // Select half the fleet (at least one host) to exercise the predicate.
        let selected: std::collections::BTreeSet<String> = (0..n.max(1))
            .step_by(2)
            .map(|i| format!("host-{i:04}"))
            .collect();
        scoped.throughput(criterion::Throughput::Elements(selected.len() as u64));
        scoped.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || (build_group(n, PER_HOST_DELAY), selected.clone()),
                |(mut group, names)| async move {
                    group.lock_selected(black_box(""), &names).await;
                    group
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    scoped.finish();
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

/// Product-file counts swept for the discovery benches (the addon-loop length).
const PRODUCT_COUNTS: &[usize] = &[1, 4, 16, 64];

/// A per-SFTP-handshake latency. A real high-latency host pays a full round
/// trip to open the channel + request the subsystem, which dwarfs the tiny cost
/// of an already-open read; the delay is set well above the mock's per-read
/// bookkeeping so the bench isolates the handshake-count difference (batched
/// pays it once per probe, per-op pays it once per read).
const HANDSHAKE_LATENCY: Duration = Duration::from_millis(2);

/// Builds a SLES host with `k` addon product files, each read during discovery,
/// with a per-SFTP-session handshake `delay`.
fn discovery_host(k: usize, delay: Duration) -> MockConnection {
    let base = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
    let mut entries: Vec<String> = vec!["SLES.prod".to_owned()];
    for i in 0..k {
        entries.push(format!("addon-{i:03}.prod"));
    }
    let mut conn = MockConnection::new("sles.example")
        .with_listing("/etc/products.d", entries)
        .with_link("/etc/products.d/baseproduct", "SLES.prod")
        .with_file("/etc/products.d/SLES.prod", base.to_vec())
        .with_sftp_session_delay(delay);
    for i in 0..k {
        let prod = format!(
            "<product><name>addon-{i:03}</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"
        );
        conn = conn.with_file(
            format!("/etc/products.d/addon-{i:03}.prod"),
            prod.into_bytes(),
        );
    }
    conn
}

/// The batched path (0mop.3): one SFTP handshake for the whole probe, flat in K.
fn bench_discovery_parse_system(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("discovery/parse_system");
    for &k in PRODUCT_COUNTS {
        g.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            b.to_async(&rt).iter_batched(
                || discovery_host(k, HANDSHAKE_LATENCY),
                |mut conn| async move {
                    let _ = parse_system(black_box(&mut conn)).await;
                    conn
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

/// The per-op counterfactual: reading the same K+1 product files one open at a
/// time pays one handshake per read, growing linearly in K — the cost the
/// batched path amortizes away.
fn bench_discovery_per_op_reads(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("discovery/per_op_reads");
    for &k in PRODUCT_COUNTS {
        g.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            b.to_async(&rt).iter_batched(
                || discovery_host(k, HANDSHAKE_LATENCY),
                |mut conn| async move {
                    let _ = conn.sftp_open(Path::new("/etc/products.d/SLES.prod")).await;
                    for i in 0..k {
                        let p = format!("/etc/products.d/addon-{i:03}.prod");
                        let _ = conn.sftp_open(black_box(Path::new(&p))).await;
                    }
                    conn
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

/// Sizes of the *pre-existing* history the `add_history` bench appends onto.
const HISTORY_SIZES: &[usize] = &[0, 1_000, 100_000];

/// `history/append` (0mop.5): the cost of recording **one** history entry as a
/// function of how large the log already is. The old read-concatenate-rewrite
/// emulation grew ~linearly in existing size (it downloaded and re-uploaded the
/// whole file every time); the append primitive is flat — one append regardless
/// of prior size. A flat curve here is the whole point of the change; the
/// deterministic oracle is the per-entry append call-count asserted in
/// `tests/history_append.rs`, this only measures the scaling shape.
fn bench_history_append(c: &mut Criterion) {
    let rt = rt();
    let mut g = c.benchmark_group("history/append");
    for &existing in HISTORY_SIZES {
        g.bench_with_input(
            BenchmarkId::from_parameter(existing),
            &existing,
            |b, &existing| {
                b.to_async(&rt).iter_batched(
                    || {
                        // A log already holding `existing` entries.
                        let mut prior = Vec::new();
                        for i in 0..existing {
                            prior.extend_from_slice(format!("{i}:seed:noop\n").as_bytes());
                        }
                        let conn =
                            MockConnection::new("host-0000").with_file("/var/log/mtui.log", prior);
                        Target::with_connection(
                            "host-0000",
                            TargetState::Enabled,
                            ExecutionMode::Parallel,
                            Box::new(conn),
                        )
                    },
                    |mut target| async move {
                        target
                            .add_history(black_box(&["install".to_owned(), "pkg".to_owned()]))
                            .await;
                        target
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_fanout_run,
    bench_fanout_run_bounded,
    bench_sftp,
    bench_lock,
    bench_report_locks,
    bench_discovery_parse_system,
    bench_discovery_per_op_reads,
    bench_history_append
);
criterion_main!(benches);
