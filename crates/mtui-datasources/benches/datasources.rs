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
//! The oqa-search fan-out is measured as a request-count invariant in the
//! integration tests rather than timed here (its wall-clock is dominated by the
//! mock server; a count is the honest regression oracle for parallelize
//! (0mop.7)). The gitea approval flow (`gitea/approve`) is timed here as a
//! *supplementary* high-latency signal for 0mop.8 — but the authoritative
//! regression gate remains the request-count oracles in `tests/gitea.rs`
//! (`approve_request_count` etc.), since the bench wall-clock is dominated by
//! the mock server's simulated latency. See `plans/perf-baseline-0mop1.md`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mtui_datasources::gitea::{Gitea, assign_marker};
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

criterion_group!(
    benches,
    bench_refhosts_parse,
    bench_refhosts_search,
    bench_http_client_new,
    bench_gitea_approve
);
criterion_main!(benches);
