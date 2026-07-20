//! Perf baseline for testreport metadata parsing (mtui-rs-0mop.1) plus the
//! blocking-filesystem paths converted in mtui-rs-0mop.9.
//!
//! Measurement-only, offline. `metadata/parse` measures `JSONParser::parse_str`
//! populating a fresh `TestReportBase` from the golden `metadata.json` fixture —
//! the parse hot path on the testreport-download workflow (0mop.12).
//!
//! The `fs/*` benches accompany 0mop.9: they measure the heavy filesystem
//! operations that were moved off the async worker (recursive checkout deletion
//! and the log-download write fan-out). They are wall-clock advisory — the
//! deterministic gate for 0mop.9 is the `fs_responsiveness` heartbeat test — but
//! they quantify the per-op cost on the target host and confirm the off-worker
//! path adds no throughput regression versus a straight blocking call.

use std::hint::black_box;

use async_trait::async_trait;
use criterion::{Criterion, criterion_group, criterion_main};
use mtui_config::options::Config;
use mtui_testreport::{BytesFetcher, ErrorMode, JSONParser, TestReportBase, download_logs};
use mtui_types::Test;

/// The golden metadata.json pinned in the fixtures tree, embedded at compile
/// time so the bench needs no runtime file I/O.
const METADATA_JSON: &str = include_str!("../tests/fixtures/metadata/metadata.json");

fn bench_metadata_parse(c: &mut Criterion) {
    c.bench_function("metadata/parse", |b| {
        b.iter(|| {
            let mut report = TestReportBase::new(Config::default());
            JSONParser::parse_str(&mut report, black_box(METADATA_JSON))
                .expect("golden metadata.json parses");
            black_box(report)
        });
    });
}

/// Populates `dir` with `files` regular files spread across `dirs` subdirectories
/// so `remove_dir_all` has a non-trivial recursive tree to walk.
fn build_tree(dir: &std::path::Path, dirs: usize, files_per_dir: usize) {
    for d in 0..dirs {
        let sub = dir.join(format!("d{d}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..files_per_dir {
            std::fs::write(sub.join(format!("f{f}.log")), b"payload").unwrap();
        }
    }
}

/// `fs/remove_tree`: recursive deletion of a checkout-sized tree via
/// `tokio::fs::remove_dir_all` (the lifecycle/regenerate path). Each iteration
/// rebuilds the tree (setup, untimed via `iter_batched`) then times the delete.
fn bench_remove_tree(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("fs");
    for &(dirs, files) in &[(4usize, 16usize), (16, 64)] {
        let total = dirs * files;
        group.bench_function(format!("remove_tree/{total}"), |b| {
            b.iter_batched(
                || {
                    let tmp = tempfile::tempdir().unwrap();
                    build_tree(tmp.path(), dirs, files);
                    tmp
                },
                |tmp| {
                    let path = tmp.path().to_path_buf();
                    rt.block_on(async move {
                        tokio::fs::remove_dir_all(black_box(&path)).await.unwrap();
                    });
                    tmp // dropped after timing (already empty)
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// A zero-latency fetcher so `fs/download_write` measures the write fan-out (the
/// `spawn_blocking` atomic-write path), not network cost.
struct InstantFetcher;

#[async_trait]
impl BytesFetcher for InstantFetcher {
    async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
        Ok(vec![0u8; 4096])
    }
}

fn dl_test(name: &str) -> Test {
    Test::new(
        name,
        "passed",
        42,
        "x86_64",
        std::collections::BTreeMap::new(),
    )
}

/// `fs/download_write`: the `download_logs` fan-out writing N logs through the
/// off-worker atomic-write path. Times fetch(instant)+write across a fresh
/// tempdir per iteration.
fn bench_download_write(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("fs");
    for &hosts in &[8usize, 32usize] {
        // Each host contributes two writable logs (install + ltp).
        let connectors: Vec<(String, Vec<Test>)> = (0..hosts)
            .map(|h| {
                (
                    format!("http://h{h}"),
                    vec![dl_test("install_kernel"), dl_test("ltp")],
                )
            })
            .collect();
        group.bench_function(format!("download_write/{}", hosts * 2), |b| {
            b.iter_batched(
                || tempfile::tempdir().unwrap(),
                |tmp| {
                    let res = tmp.path().join("results");
                    let inst = tmp.path().join("install");
                    rt.block_on(async {
                        download_logs(
                            &InstantFetcher,
                            black_box(&connectors),
                            &res,
                            &inst,
                            ErrorMode::Tolerant,
                        )
                        .await
                        .unwrap();
                    });
                    tmp
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_metadata_parse,
    bench_remove_tree,
    bench_download_write
);
criterion_main!(benches);
