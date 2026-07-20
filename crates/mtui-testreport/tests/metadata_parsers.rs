//! Ported from upstream `tests/test_metadata_parsers.py`.
//!
//! Covers only the JSON-related surface that survives in the Rust port:
//! [`JSONParser`] and [`patchinfo_titles`]. Upstream's `ReducedMetadataParser`
//! (dropped legacy text-embedding) and the `*repoparse` helpers (which belong to
//! the products/report tasks) are intentionally not exercised here.

use std::collections::{HashMap, HashSet};

use mtui_config::options::Config;
use mtui_testreport::{JSONParser, ReducedMetadataParser, TestReportBase, patchinfo_titles};

/// A bare [`TestReportBase`] to parse into — the analogue of upstream's
/// `FakeTestreport` / `MagicMock` results object.
fn empty_report() -> TestReportBase {
    TestReportBase::new(Config::default())
}

/// Golden fixture: the same `metadata.json` upstream pins in
/// `tests/fixtures/metadata/metadata.json`.
const METADATA_JSON: &str = include_str!("fixtures/metadata/metadata.json");

#[test]
fn reduced_metadata_parser_parses_hosts_jira_bugs() {
    // Port of upstream `test_reduced_metadata_parser_parse`.
    let mut report = empty_report();

    // Hostname line.
    ReducedMetadataParser::parse(&mut report, "some text (reference host: test_host)");
    assert!(report.hostnames.contains("test_host"));

    // Jira line.
    ReducedMetadataParser::parse(&mut report, r#"Jira ABC-123 ("Test Jira issue"):"#);
    assert_eq!(report.jira["ABC-123"], "Test Jira issue");

    // Bug line.
    ReducedMetadataParser::parse(&mut report, r#"Bug 123 ("Test bug"):"#);
    assert_eq!(report.bugs["123"], "Test bug");
}

#[test]
fn reduced_metadata_parser_reads_back_the_slack_review_marker() {
    // The read-back half of the marker contract: `approve` gates on this, so
    // if the parser stops wiring the line up, an approval that should be
    // refused would sail through instead.
    let mut report = empty_report();

    ReducedMetadataParser::parse(&mut report, "Slack Review: C0123456789 1700000000.000100");

    let marker = report.slack_review.expect("marker parsed");
    assert_eq!(marker.channel, "C0123456789");
    assert_eq!(marker.ts, "1700000000.000100");
}

#[test]
fn reduced_metadata_parser_keeps_the_first_slack_marker() {
    // First-wins, matching the writer's duplicate collapsing: read and write
    // must agree on which message the gate checks.
    let mut report = empty_report();

    ReducedMetadataParser::parse(&mut report, "Slack Review: CFIRST 1.0");
    ReducedMetadataParser::parse(&mut report, "Slack Review: CSECOND 2.0");

    let marker = report.slack_review.expect("marker parsed");
    assert_eq!(marker.channel, "CFIRST");
}

#[test]
fn reduced_metadata_parser_ignores_a_malformed_slack_marker() {
    // A truncated marker points at no real message. Treating it as absent
    // makes `approve` refuse (safe); half-parsing it could make the gate check
    // the wrong message.
    let mut report = empty_report();

    ReducedMetadataParser::parse(&mut report, "Slack Review: CONLYCHANNEL");
    assert!(report.slack_review.is_none());

    ReducedMetadataParser::parse(&mut report, "Slack Review: C1 1.0 extra");
    assert!(report.slack_review.is_none());

    // A marker line must not be mistaken for the other metadata kinds.
    assert!(report.hostnames.is_empty());
    assert!(report.jira.is_empty());
    assert!(report.bugs.is_empty());
}

#[test]
fn reduced_metadata_parser_skips_placeholder_host_and_ignores_other_lines() {
    // Upstream guards `"?" not in match.group(1)`; unmatched lines are no-ops.
    let mut report = empty_report();

    ReducedMetadataParser::parse(&mut report, "text (reference host: ?)");
    assert!(report.hostnames.is_empty());

    ReducedMetadataParser::parse(&mut report, "a line with no metadata at all");
    assert!(report.hostnames.is_empty());
    assert!(report.jira.is_empty());
    assert!(report.bugs.is_empty());
}

#[test]
fn json_parser_parses_golden_fixture() {
    // Port of the JSON half of upstream `test_parse_new` (the reduced text
    // parser it also exercises is dropped, so hostnames are out of scope).
    let mut report = empty_report();
    JSONParser::parse_str(&mut report, METADATA_JSON).expect("valid metadata.json");

    assert_eq!(report.rating.as_deref(), Some("low"));
    assert_eq!(
        report.bugs,
        HashMap::from([("12345".to_owned(), "Description not available".to_owned())])
    );
    assert_eq!(report.category, "recommended");
    assert_eq!(
        report.rrid.as_ref().map(ToString::to_string),
        Some("SUSE:Maintenance:24993:275518".to_owned())
    );
    assert_eq!(
        report.jira,
        HashMap::from([(
            "SLE-22357".to_owned(),
            "Description not available".to_owned()
        )])
    );
    assert_eq!(
        report.repository,
        "http://download.suse.de/ibs/SUSE:/Maintenance:/24993/"
    );
    // New-format envelope carries no reviewer field: it stays the default.
    assert_eq!(report.reviewer, "");
    assert_eq!(report.packager, "slemke@suse.com");
    assert_eq!(
        report.products,
        vec![
            "SLE-Module-Development-Tools-OBS 15-SP4 (aarch64, ppc64le, s390x, x86_64)".to_owned(),
            "SLE-Module-Python2 15-SP3 (aarch64, ppc64le, s390x, x86_64)".to_owned(),
        ]
    );
    assert_eq!(
        report.testplatforms,
        vec![
            "base=sles(major=15,minor=sp3);arch=[s390x,x86_64];addon=python2(major=15,minor=sp3)"
                .to_owned(),
            "base=sles(major=15,minor=sp4);arch=[s390x,x86_64];addon=Development-Tools-OBS(major=15,minor=sp4)"
                .to_owned(),
            "base=SLES(major=15,minor=SP3);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-python2(major=15,minor=SP3)"
                .to_owned(),
            "base=SLES(major=15,minor=SP4);arch=[aarch64,ppc64le,s390x,x86_64];addon=sle-module-development-tools-obs(major=15,minor=SP4)"
                .to_owned(),
        ]
    );
    // Nested packages: one entry per product, each with its own package set.
    assert_eq!(
        report.packages,
        HashMap::from([
            (
                "15-SP3".to_owned(),
                HashMap::from([(
                    "sle-module-python2-release".to_owned(),
                    "15.3-150300.59.4.1".to_owned()
                )])
            ),
            (
                "15-SP4".to_owned(),
                HashMap::from([(
                    "sle-module-python2-release".to_owned(),
                    "15.3-150300.59.4.1".to_owned()
                )])
            ),
        ])
    );
}

#[test]
fn json_parser_parse_maps_every_field() {
    // Port of upstream `test_json_parser_parse`.
    let mut report = empty_report();
    let data = r#"{
        "jira": ["ABC-123"],
        "bugs": ["123"],
        "rrid": "SUSE:Maintenance:1:1",
        "packager": "test_packager",
        "rating": "test_rating",
        "repository": "test_repository",
        "category": "test_category",
        "testplatform": ["test_platform"],
        "products": ["test_product"],
        "id": "test_id",
        "gitea_pr": "test_gitea_pr",
        "gitea_pr_api": "test_gitea_pr_api",
        "packages": {"test_prod": ["test_pkg 1.0 1.0"]},
        "repositories": ["test_repo"]
    }"#;

    JSONParser::parse_str(&mut report, data).expect("valid json");

    assert_eq!(report.jira["ABC-123"], "Description not available");
    assert_eq!(report.bugs["123"], "Description not available");
    assert_eq!(
        report.rrid.as_ref().map(ToString::to_string),
        Some("SUSE:Maintenance:1:1".to_owned())
    );
    assert_eq!(report.packager, "test_packager");
    assert_eq!(report.rating.as_deref(), Some("test_rating"));
    assert_eq!(report.repository, "test_repository");
    assert_eq!(report.category, "test_category");
    assert_eq!(report.testplatforms, vec!["test_platform".to_owned()]);
    assert_eq!(report.products, vec!["test_product".to_owned()]);
    assert_eq!(report.realid.as_deref(), Some("test_id"));
    assert_eq!(report.giteapr.as_deref(), Some("test_gitea_pr"));
    assert_eq!(report.giteaprapi.as_deref(), Some("test_gitea_pr_api"));
    assert_eq!(report.packages["test_prod"]["test_pkg"], "1.0");
    assert_eq!(report.repositories, HashSet::from(["test_repo".to_owned()]));
}

#[test]
fn json_parser_drops_injection_shaped_package_names() {
    // A package name carrying shell metacharacters must be dropped at ingestion
    // (it is interpolated into root remote commands), while valid siblings in
    // the same product set are retained.
    let mut report = empty_report();
    let data = r#"{
        "rrid": "SUSE:Maintenance:1:1",
        "packages": {"prod": [
            "bash 1.0 5.1-1",
            "foo;rm 1.0 2.0",
            "kernel-default 1.0 5.14.21-150500"
        ]}
    }"#;

    JSONParser::parse_str(&mut report, data).expect("valid json");

    let prod = &report.packages["prod"];
    assert!(prod.contains_key("bash"), "valid name dropped: {prod:?}");
    assert!(
        prod.contains_key("kernel-default"),
        "valid name dropped: {prod:?}"
    );
    assert!(
        !prod.keys().any(|k| k.contains(';')),
        "injection-shaped name retained: {prod:?}"
    );
    assert_eq!(prod.len(), 2);
}

#[test]
fn json_parser_tolerates_missing_optional_keys() {
    // Port of upstream
    // `test_json_parser_parse_tolerates_missing_optional_keys`: absent list/dict
    // keys and an explicit null must not raise and must yield empty containers.
    let mut report = empty_report();
    let data = r#"{"rrid": "SUSE:Maintenance:1:1", "packages": null}"#;

    JSONParser::parse_str(&mut report, data).expect("valid json");

    assert!(report.jira.is_empty());
    assert!(report.bugs.is_empty());
    assert!(report.packages.is_empty());
    assert!(report.repositories.is_empty());
}

/// Golden snapshot of the parsed `metadata.json` envelope.
///
/// The field-by-field assertions in `json_parser_parses_golden_fixture` pin
/// individual values; this freezes the whole parsed view of the pinned upstream
/// fixture as one stable rendering, so a regression in the JSON envelope -> struct
/// mapping surfaces as a single reviewable snapshot diff. `HashMap`/`HashSet`
/// fields are rendered in sorted order to keep the snapshot deterministic.
#[test]
fn parsed_metadata_json_is_stable() {
    let mut report = empty_report();
    JSONParser::parse_str(&mut report, METADATA_JSON).expect("valid metadata.json");

    let mut out = String::new();
    let mut push = |k: &str, v: String| out.push_str(&format!("{k}: {v}\n"));

    push(
        "rrid",
        report
            .rrid
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
    );
    push("realid", report.realid.clone().unwrap_or_default());
    push("category", report.category.clone());
    push("rating", report.rating.clone().unwrap_or_default());
    push("packager", report.packager.clone());
    push("reviewer", report.reviewer.clone());
    push("repository", report.repository.clone());

    let sorted = |m: &HashMap<String, String>| {
        let mut v: Vec<_> = m.iter().map(|(k, val)| format!("{k}={val}")).collect();
        v.sort();
        v.join(", ")
    };
    push("bugs", sorted(&report.bugs));
    push("jira", sorted(&report.jira));

    let mut products = report.products.clone();
    products.sort();
    push("products", products.join(" | "));

    let mut platforms = report.testplatforms.clone();
    platforms.sort();
    push("testplatforms", platforms.join(" | "));

    let mut pkgs: Vec<String> = report
        .packages
        .iter()
        .flat_map(|(prod, set)| set.iter().map(move |(n, ver)| format!("{prod}/{n}={ver}")))
        .collect();
    pkgs.sort();
    push("packages", pkgs.join(", "));

    insta::assert_snapshot!("parsed_metadata_json", out);
}

#[test]
fn patchinfo_titles_maps_ids_to_titles() {
    // Port of upstream `test_patchinfo_titles`.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("patchinfo.xml"),
        r#"<patchinfo>
          <issue tracker="bnc" id="1260938">Deprecate SHA1</issue>
          <issue tracker="bnc" id="1265607">All-Zero HMAC Key Detected</issue>
          <issue tracker="jsc" id="PED-1">A feature</issue>
        </patchinfo>"#,
    )
    .expect("write patchinfo");

    let titles = patchinfo_titles(dir.path());
    assert_eq!(titles["1260938"], "Deprecate SHA1");
    assert_eq!(titles["1265607"], "All-Zero HMAC Key Detected");
    assert_eq!(titles["PED-1"], "A feature");
}

#[test]
fn patchinfo_titles_absent_is_empty() {
    // Port of upstream `test_patchinfo_titles_absent`: no file -> empty map.
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(patchinfo_titles(dir.path()).is_empty());
}

#[test]
fn patchinfo_titles_malformed_is_empty() {
    // Port of upstream `test_patchinfo_titles_malformed`: unparseable XML must
    // degrade to an empty map rather than an error.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("patchinfo.xml"), "<patchinfo><issue ")
        .expect("write patchinfo");
    assert!(patchinfo_titles(dir.path()).is_empty());
}
