//! The equivalence test for the document-based ingest path.
//!
//! For each pair harvested into `tests/fixtures/pairs/` (see `PROVENANCE.md`
//! alongside them), builds a [`TestReportBase`] two ways — via today's
//! [`TestReport::read`] over the on-disk `metadata.json`/`log`, and via
//! [`apply_document`] over `document.json` — and asserts field-for-field
//! equality across every field in the mapping table, with the four documented
//! divergences asserted *positively*, never skipped:
//!
//! 1. **`hostnames`** — empty from the document (`testing.install.checks[]` is
//!    absent on every document in the current corpus) while the SVN side
//!    carries the real reference hosts scraped from the log.
//! 2. **`bugs`/`jira` titles** — key sets equal; the SVN side is exactly the
//!    `NO_DESCRIPTION` placeholder (these fixtures commit no `patchinfo.xml`),
//!    the document side the real title.
//! 3. **`repositories`** — the document side must be a (possibly proper)
//!    subset of metadata's (a target can be dropped from the document for
//!    unresolvable binaries while metadata still lists its repository URL).
//!    Every pair in this set happens to show equality — the boundary case of
//!    "subset" — because harvesting one that shows a *proper* subset also
//!    surfaced a second, undocumented gap (see `PROVENANCE.md`'s "Known gap
//!    not exercised here": some SLFO `install.targets[].binaries` omit
//!    non-package build artifacts, e.g. `*-image` packages, that metadata's
//!    separate `binaries` block still records) which would have made this
//!    test assert something it cannot yet explain. The subset assertion
//!    itself is unconditional and would still catch a regression to "superset"
//!    or "disjoint".
//! 4. **`generated_at`** — not stored on [`TestReportBase`] at all, so there is
//!    nothing to assert; noted here so nobody adds it.
//!
//! `update_repos` is asserted equal only when its declared input
//! (`repositories`) is itself equal between the two sides — on this
//! fixture set that is always, but the guard stays because a future subset
//! divergence would otherwise fail this assertion for a reason that is really
//! #3 resurfacing, not a new bug.

use std::collections::BTreeSet;
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

/// Builds a [`TestReportBase`] the way today's lifecycle does: `TestReport::read`
/// over the checked-out `log`/`metadata.json`.
fn build_via_svn(pair: &Pair) -> Box<dyn TestReport + Send + Sync> {
    let mut report = new_report(pair.kind, Config::default());
    let trpath = pair_dir(pair.rrid).join("log");
    report
        .read(&trpath)
        .unwrap_or_else(|e| panic!("{}: SVN-side read failed: {e}", pair.rrid));
    report
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
    report.base_mut().path = Some(pair_dir(pair.rrid).join("log"));
    let repos = report.update_repos_parser();
    report.base_mut().update_repos = repos;
    (report, document)
}

const NO_DESCRIPTION: &str = "Description not available";

#[test]
fn equivalence_across_every_harvested_pair() {
    for pair in PAIRS {
        let svn_report = build_via_svn(pair);
        let (doc_report, _document) = build_via_document(pair);
        let svn: &TestReportBase = svn_report.base();
        let doc: &TestReportBase = doc_report.base();
        let id = pair.rrid;

        assert_eq!(svn.rrid, doc.rrid, "{id}: rrid");
        assert_eq!(svn.realid, doc.realid, "{id}: realid");
        assert_eq!(svn.packager, doc.packager, "{id}: packager");
        assert_eq!(svn.rating, doc.rating, "{id}: rating");
        assert_eq!(svn.category, doc.category, "{id}: category");
        assert_eq!(svn.repository, doc.repository, "{id}: repository");
        assert_eq!(svn.products, doc.products, "{id}: products");
        assert_eq!(svn.testplatforms, doc.testplatforms, "{id}: testplatforms");
        assert_eq!(svn.packages, doc.packages, "{id}: packages");
        assert_eq!(svn.composed, doc.composed, "{id}: composed");
        assert_eq!(svn.giteapr, doc.giteapr, "{id}: giteapr");
        assert_eq!(svn.giteaprapi, doc.giteaprapi, "{id}: giteaprapi");
        assert_eq!(svn.giteacohash, doc.giteacohash, "{id}: giteacohash");
        assert_eq!(svn.update_source, doc.update_source, "{id}: update_source");
        assert_eq!(svn.slack_review, doc.slack_review, "{id}: slack_review");
        assert_eq!(svn.reviewer, doc.reviewer, "{id}: reviewer");

        // Divergence #3: the document's repositories are a (possibly proper)
        // subset of metadata's.
        assert!(
            doc.repositories.is_subset(&svn.repositories),
            "{id}: document repositories must be a subset of the SVN side's \
             (doc={:?}, svn={:?})",
            doc.repositories,
            svn.repositories
        );
        // `update_repos` is derived from `repositories` (untouched by this
        // mapping) — asserting it equal only makes sense when its input is; a
        // diverging `repositories` (as on `SUSE:SLFO:1.2:7810`) legitimately
        // cascades.
        if svn.repositories == doc.repositories {
            assert_eq!(svn.update_repos, doc.update_repos, "{id}: update_repos");
        }

        // Divergence #1: hostnames. Positively asserted both ways so the
        // divergence cannot be mistaken for a mapping bug nor a fixture
        // artifact: the SVN side's log genuinely carries reference-host
        // lines, and the document side's `testing.install.checks[]` is
        // genuinely absent from every document in today's corpus.
        assert!(
            doc.hostnames.is_empty(),
            "{id}: document hostnames must be empty (divergence #1)"
        );
        assert!(
            !svn.hostnames.is_empty(),
            "{id}: SVN-side hostnames unexpectedly empty — fixture no longer \
             exercises divergence #1"
        );

        // Divergence #2: bug/jira key sets equal; titles diverge in the
        // document's favor. These fixtures commit no `patchinfo.xml`, so the
        // SVN side is exactly the `NO_DESCRIPTION` placeholder.
        let svn_bug_keys: BTreeSet<&String> = svn.bugs.keys().collect();
        let doc_bug_keys: BTreeSet<&String> = doc.bugs.keys().collect();
        assert_eq!(svn_bug_keys, doc_bug_keys, "{id}: bug key sets");
        let svn_jira_keys: BTreeSet<&String> = svn.jira.keys().collect();
        let doc_jira_keys: BTreeSet<&String> = doc.jira.keys().collect();
        assert_eq!(svn_jira_keys, doc_jira_keys, "{id}: jira key sets");
        for (bug_id, svn_title) in &svn.bugs {
            assert_eq!(
                svn_title, NO_DESCRIPTION,
                "{id}: bug {bug_id} SVN-side title should be the placeholder"
            );
            let doc_title = &doc.bugs[bug_id];
            assert_ne!(
                doc_title, svn_title,
                "{id}: bug {bug_id} document title should be the real title, \
                 not the placeholder"
            );
        }
        for (jira_id, svn_title) in &svn.jira {
            assert_eq!(
                svn_title, NO_DESCRIPTION,
                "{id}: jira {jira_id} SVN-side title should be the placeholder"
            );
            let doc_title = &doc.jira[jira_id];
            assert_ne!(
                doc_title, svn_title,
                "{id}: jira {jira_id} document title should be the real title, \
                 not the placeholder"
            );
        }
    }
}
