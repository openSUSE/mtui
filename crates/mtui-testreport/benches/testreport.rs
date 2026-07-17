//! Perf baseline for testreport metadata parsing (mtui-rs-0mop.1).
//!
//! Measurement-only, offline. `metadata/parse` measures `JSONParser::parse_str`
//! populating a fresh `TestReportBase` from the golden `metadata.json` fixture —
//! the parse hot path on the testreport-download workflow (0mop.12). The heavier
//! download/checkout paths are I/O-bound (SVN/HTTP) and are covered by the
//! request-count / call-count oracles in the integration tests rather than timed
//! here (see plans/perf-baseline-0mop1.md).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use mtui_config::options::Config;
use mtui_testreport::{JSONParser, TestReportBase};

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

criterion_group!(benches, bench_metadata_parse);
criterion_main!(benches);
