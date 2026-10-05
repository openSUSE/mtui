//! Golden projection of the document-based ingest path.
//!
//! For each pair in `tests/fixtures/pairs/` (see `PROVENANCE.md` alongside
//! them), builds a [`TestReportBase`] via [`apply_document`] over
//! `document.json` and `update_repos_parser`, and pins every field they set as
//! an `insta` snapshot. The values were agreed against the retired text
//! template reader before it was removed: reference `hostnames` are empty (the
//! document carries no `testing.install.checks[]` yet), bug/jira titles are the
//! document's real titles, and both OBS pairs yield a non-empty `update_repos`.

use std::path::{Path, PathBuf};

use mtui_config::Config;
use mtui_testreport::testreport::TestReportBase;
use mtui_testreport::{ObsReport, SlReport, TestReport, apply_document};
use mtui_types::enums::RequestKind;
use mtui_types::report_document::ReportDocument;

struct Pair {
    rrid: &'static str,
    kind: RequestKind,
}

const PAIRS: &[Pair] = &[
    Pair {
        rrid: "SUSE:Maintenance:46456:424163",
        kind: RequestKind::Maintenance,
    },
    Pair {
        rrid: "SUSE:Maintenance:46572:423943",
        kind: RequestKind::Maintenance,
    },
    Pair {
        rrid: "SUSE:SLFO:1.2:7787",
        kind: RequestKind::Slfo,
    },
    Pair {
        rrid: "SUSE:SLFO:1.2:7810",
        kind: RequestKind::Slfo,
    },
];

fn pair_dir(rrid: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pairs")
        .join(rrid)
}

fn new_report(kind: RequestKind, config: Config) -> Box<dyn TestReport + Send + Sync> {
    match kind {
        RequestKind::Slfo => Box::new(SlReport::new(config)),
        RequestKind::Pi => panic!("no PI fixture in this pair set (see PROVENANCE.md)"),
        RequestKind::Maintenance => Box::new(ObsReport::new(config)),
    }
}

/// Builds a [`TestReportBase`] via the v2 document, mirroring
/// `lifecycle::load_via_document`'s tail (`apply_document`, then `path` +
/// `update_repos_parser()`) without the network/checkout machinery.
fn build_via_document(pair: &Pair) -> (Box<dyn TestReport + Send + Sync>, ReportDocument) {
    let mut report = new_report(pair.kind, Config::default());
    let raw = std::fs::read_to_string(pair_dir(pair.rrid).join("document.json"))
        .unwrap_or_else(|e| panic!("{}: reading document.json: {e}", pair.rrid));
    let document: ReportDocument = raw
        .parse()
        .unwrap_or_else(|e| panic!("{}: document.json did not parse: {e}", pair.rrid));
    apply_document(report.base_mut(), &document);
    report.base_mut().document = Some(document.clone());
    report.base_mut().path = Some(pair_dir(pair.rrid).join("log"));
    let repos = report.update_repos_parser();
    report.base_mut().update_repos = repos;
    (report, document)
}

/// Every field `apply_document` and `update_repos_parser` set, in a stable,
/// sorted text form (`TestReportBase` has no `Debug`).
fn projection(base: &TestReportBase) -> String {
    use std::fmt::Write as _;

    fn sorted<T: Ord>(items: impl IntoIterator<Item = T>) -> Vec<T> {
        let mut v: Vec<T> = items.into_iter().collect();
        v.sort();
        v
    }

    let mut out = String::new();
    let mut line = |name: &str, value: String| {
        writeln!(out, "{name}: {value}").unwrap();
    };
    line("rrid", format!("{:?}", base.rrid));
    line("realid", format!("{:?}", base.realid));
    line("packager", format!("{:?}", base.packager));
    line("rating", format!("{:?}", base.rating));
    line("category", format!("{:?}", base.category));
    line("repository", format!("{:?}", base.repository));
    line("products", format!("{:?}", base.products));
    line("testplatforms", format!("{:?}", base.testplatforms));
    line("giteapr", format!("{:?}", base.giteapr));
    line("giteaprapi", format!("{:?}", base.giteaprapi));
    line("giteacohash", format!("{:?}", base.giteacohash));
    line("update_source", format!("{:?}", base.update_source));
    line("slack_review", format!("{:?}", base.slack_review));
    line("reviewer", format!("{:?}", base.reviewer));
    line("repositories", format!("{:?}", sorted(&base.repositories)));
    line("hostnames", format!("{:?}", sorted(&base.hostnames)));
    line("bugs", format!("{:?}", sorted(&base.bugs)));
    line("jira", format!("{:?}", sorted(&base.jira)));
    let packages: Vec<_> = sorted(
        base.packages
            .iter()
            .map(|(k, v)| (k, sorted(v.iter().collect::<Vec<_>>()))),
    );
    line("packages", format!("{packages:?}"));
    let composed: Vec<_> = sorted(
        base.composed
            .iter()
            .map(|(k, v)| (format!("{k:?}"), v.iter().collect::<Vec<_>>())),
    );
    line("composed", format!("{composed:?}"));
    let update_repos: Vec<_> = sorted(base.update_repos.iter().map(|(k, v)| (format!("{k:?}"), v)));
    line("update_repos", format!("{update_repos:?}"));
    out
}

#[test]
fn ingest_golden_per_pair() {
    for pair in PAIRS {
        let (report, _document) = build_via_document(pair);
        let name = format!("ingest_golden_{}", pair.rrid.replace(':', "_"));
        insta::assert_snapshot!(name, projection(report.base()));
    }
}
