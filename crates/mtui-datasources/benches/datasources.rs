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
//! The oqa-search fan-out's authoritative regression gate is the request-count
//! and order oracle in `tests/oqa_search.rs` (a count/order is the honest signal
//! for parallelize, 0mop.7). `oqa/single_incidents` is timed here as a
//! supplementary high-latency signal contrasting a serial bound with a parallel
//! bound: with a per-response delay the sequential curve is additive while the
//! bounded-parallel curve is ~flat. The gitea approval flow (`gitea/approve`) is
//! the analogous supplementary signal for 0mop.8. Bench wall-clock is dominated
//! by the mock server's simulated latency, so the count oracles remain the gate.
//! See `plans/perf-baseline-0mop1.md`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_datasources::gitea::{Gitea, assign_marker};
use mtui_datasources::oqa_search::search::single_incidents;
use mtui_datasources::{Attributes, HttpClient, Refhosts, VerifyPolicy};
use mtui_types::load_refhosts;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// Simulated per-response latency for the gitea approval bench, so the
/// round-trip *count* (the thing 0mop.8 changes) shows up in wall-clock. Small
/// enough to keep the bench quick; the count oracle is the real gate.
const GITEA_RESPONSE_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// Mount a mock Gitea PR whose comments show the session user assigned (so a
/// happy-path `approve` proceeds), each response delayed by
/// [`GITEA_RESPONSE_DELAY`]. Returns a ready `Gitea` client pointed at it.
async fn gitea_approve_fixture(server: &MockServer) -> Gitea {
    let comments = json!([{
        "id": 1,
        "body": assign_marker("benchuser", "qam-sle"),
        "updated_at": "2024-01-01T00:00:00+00:00",
    }]);
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/owner/repo/issues/1/comments"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(comments)
                .set_delay(GITEA_RESPONSE_DELAY),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/owner/repo/issues/1/comments"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "id": 999 }))
                .set_delay(GITEA_RESPONSE_DELAY),
        )
        .mount(server)
        .await;
    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    let pr_api = format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri());
    Gitea::with_client(
        http,
        "tok".to_string(),
        "benchuser".to_string(),
        &pr_api,
        None,
    )
}

/// Time one happy-path `approve` against a latency-injecting mock server. Fewer
/// comment fetches (0mop.8: 2->1) means fewer round trips at `GITEA_RESPONSE_DELAY`
/// each. Supplementary to the request-count oracle.
fn bench_gitea_approve(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime builds");
    let (server, gitea) = rt.block_on(async {
        let server = MockServer::start().await;
        let gitea = gitea_approve_fixture(&server).await;
        (server, gitea)
    });
    c.bench_function("gitea/approve", |b| {
        b.to_async(&rt)
            .iter(|| async { black_box(gitea.approve(None).await).expect("approve succeeds") });
    });
    drop(server);
}

/// Per-response latency for the oqa-search bench, so the serial-vs-parallel
/// fan-out difference (0mop.7) shows up in wall-clock. The request-count/order
/// oracle in `tests/oqa_search.rs` is the real gate.
const OQA_RESPONSE_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// Number of versions to fan out over in the oqa-search bench.
const OQA_VERSIONS: usize = 8;

/// Time `single_incidents` over `OQA_VERSIONS` versions against a latency-injected
/// mock, contrasting a serial bound (1) with a parallel bound (`OQA_VERSIONS`).
/// Each version issues 2 delayed overview GETs (tokio::join!ed); serial is
/// additive across versions, bounded-parallel is ~flat.
fn bench_oqa_single_incidents(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime builds");
    let (server, uri, versions) = rt.block_on(async {
        let server = MockServer::start().await;
        let mut groups = Vec::new();
        let mut versions = Vec::new();
        for i in 0..OQA_VERSIONS {
            let ver = format!("15-SP{i}");
            groups.push(json!({
                "id": 100 + i as i64,
                "name": format!("SLE 15 SP{i} Core Incidents"),
                "template": "tpl",
            }));
            versions.push(ver);
        }
        Mock::given(method("GET"))
            .and(path("/api/v1/job_groups"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(groups)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs/overview"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([]))
                    .set_delay(OQA_RESPONSE_DELAY),
            )
            .mount(&server)
            .await;
        let uri = server.uri();
        (server, uri, versions)
    });

    let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
    let mut g = c.benchmark_group("oqa/single_incidents");
    for bound in [1usize, OQA_VERSIONS] {
        g.bench_with_input(BenchmarkId::from_parameter(bound), &bound, |b, &bound| {
            b.to_async(&rt).iter(|| async {
                black_box(single_incidents(&http, ":1:pkg", &versions, &uri, bound).await)
            });
        });
    }
    g.finish();
    drop(server);
}

criterion_group!(
    benches,
    bench_refhosts_parse,
    bench_refhosts_search,
    bench_http_client_new,
    bench_gitea_approve,
    bench_oqa_single_incidents
);
criterion_main!(benches);
