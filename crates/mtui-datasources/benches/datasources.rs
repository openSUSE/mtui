//! Perf baselines for datasource hot paths (mtui-rs-0mop.1).
//!
//! Measurement-only, fully offline. Covers the pure/deterministic hot paths the
//! remediation beads target:
//! - `refhosts/parse`: `load_refhosts` (YAML parse + location-flatten + dedup)
//!   over synthetic documents of growing size — the parse/lookup hot path
//!   (0mop.12).
//! - `refhosts/search`: `Refhosts::search` over a large host set — the O(attrs ×
//!   hosts) scan (0mop.12).
//! - `http/client_new`: constructing an `HttpClient` (which builds a fresh
//!   `reqwest::Client`, i.e. a new connection pool + TLS config each time) — the
//!   baseline for reusing one client across commands (0mop.13).
//!
//! The network-fan-out workloads (oqa-search, gitea approval) are measured as
//! request-count invariants in the integration tests rather than timed here:
//! their wall-clock is dominated by the mock server, so a count is the honest
//! regression oracle for dedup (0mop.8) / parallelize (0mop.7). See
//! `plans/perf-baseline-0mop1.md`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_datasources::{Attributes, HttpClient, Refhosts, VerifyPolicy};
use mtui_types::load_refhosts;

/// Host-count points swept for the refhosts parse/search benches.
const HOST_COUNTS: &[usize] = &[16, 128, 1024];

/// Builds a legacy location-grouped refhosts.yml string with `n` hosts spread
/// across a few location groups (so the flatten/dedup path is exercised).
fn synthetic_refhosts_yaml(n: usize) -> String {
    let mut s = String::from("group_a:\n");
    for i in 0..n {
        if i == n / 2 {
            s.push_str("group_b:\n");
        }
        let major = 12 + (i % 4);
        let minor = i % 6;
        s.push_str(&format!(
            "  - name: host-{i:05}\n    arch: {arch}\n    product:\n      name: sles\n      version:\n        major: {major}\n        minor: {minor}\n",
            arch = if i % 2 == 0 { "x86_64" } else { "aarch64" },
        ));
    }
    s
}

fn bench_refhosts_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("refhosts/parse");
    for &n in HOST_COUNTS {
        let yaml = synthetic_refhosts_yaml(n);
        g.throughput(criterion::Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &yaml, |b, yaml| {
            b.iter(|| {
                let hosts = load_refhosts(black_box(yaml)).expect("synthetic yaml parses");
                black_box(hosts)
            });
        });
    }
    g.finish();
}

fn bench_refhosts_search(c: &mut Criterion) {
    let mut g = c.benchmark_group("refhosts/search");
    for &n in HOST_COUNTS {
        let yaml = synthetic_refhosts_yaml(n);
        let hosts = load_refhosts(&yaml).expect("synthetic yaml parses");
        let rh = Refhosts::from_hosts(hosts);
        let attrs: Vec<Attributes> =
            Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        g.throughput(criterion::Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &rh, |b, rh| {
            b.iter(|| black_box(rh.search(black_box(attrs.as_slice()))));
        });
    }
    g.finish();
}

fn bench_http_client_new(c: &mut Criterion) {
    c.bench_function("http/client_new", |b| {
        b.iter(|| {
            let client =
                HttpClient::new(black_box(VerifyPolicy::Default(true))).expect("client builds");
            black_box(client)
        });
    });
}

criterion_group!(
    benches,
    bench_refhosts_parse,
    bench_refhosts_search,
    bench_http_client_new
);
criterion_main!(benches);
