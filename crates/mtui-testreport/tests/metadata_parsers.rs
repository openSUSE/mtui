//! Metadata parser tests: [`ReducedMetadataParser`], [`JSONParser`] and
//! [`patchinfo_titles`]. The `*repoparse` helpers are the report side and are
//! covered by `tests/repoparse.rs`.

use std::collections::{HashMap, HashSet};

use mtui_config::options::Config;
use mtui_testreport::testreport::packages_for_map;
use mtui_testreport::{JSONParser, ReducedMetadataParser, TestReportBase, patchinfo_titles};

/// A bare [`TestReportBase`] to parse into.
fn empty_report() -> TestReportBase {
    TestReportBase::new(Config::default())
}

/// Golden fixture: `tests/fixtures/metadata/metadata.json`.
const METADATA_JSON: &str = include_str!("fixtures/metadata/metadata.json");

/// Real-metadata fixture: `tests/fixtures/metadata/slfo_metadata.json`.
///
/// Captured from TeReGen for `SUSE:SLFO:1.1:418286` on 2026-08-11,
/// **field-for-field** identical to the fetched record but for `packager`,
/// replaced with `someone@suse.com` so no named individual's address is
/// published. It is **not** byte-for-byte: the record arrives as one ~950-byte
/// line and was re-serialized (4-space indent, sorted keys) to match its golden
/// sibling `metadata.json`, so a diff against a fresh fetch shows a whole-file
/// reformat. Compare parsed values, not bytes.
///
/// Do not "tidy" any value: the point is that the domain values are observed
/// rather than guessed — #396 shipped a synthetic SLFO probe that guessed the
/// map key wrong, and only a real record settled it (#397).
const SLFO_METADATA_JSON: &str = include_str!("fixtures/metadata/slfo_metadata.json");

#[test]
fn reduced_metadata_parser_parses_hosts_jira_bugs() {
    let mut report = empty_report();

    ReducedMetadataParser::parse(&mut report, "some text (reference host: test_host)");
    assert!(report.hostnames.contains("test_host"));

    ReducedMetadataParser::parse(&mut report, r#"Jira ABC-123 ("Test Jira issue"):"#);
    assert_eq!(report.jira["ABC-123"], "Test Jira issue");

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
    // A truncated marker points at no real message: treating it as absent makes
    // `approve` refuse, where half-parsing would check the wrong message.
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
    // A `?` host is a placeholder; unmatched lines are no-ops.
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
    // Hostnames are out of scope: the reduced text parser is exercised above.
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
        "packages": {"test_prod": ["test_pkg 1.0 2.0"]},
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
    // The two tokens after the name must differ: with `"test_pkg 1.0 1.0"` this
    // could not tell version-from-token[1] from version-from-token[2].
    assert_eq!(report.packages["test_prod"]["test_pkg"], "2.0");
    assert_eq!(report.repositories, HashSet::from(["test_repo".to_owned()]));
}

#[test]
fn json_parser_drops_injection_shaped_package_names() {
    // A name with shell metacharacters must be dropped at ingestion (it reaches
    // root remote commands), while valid siblings in the set are retained.
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
    // Absent list/dict keys and an explicit null must not raise and must
    // yield empty containers.
    let mut report = empty_report();
    let data = r#"{"rrid": "SUSE:Maintenance:1:1", "packages": null}"#;

    JSONParser::parse_str(&mut report, data).expect("valid json");

    assert!(report.jira.is_empty());
    assert!(report.bugs.is_empty());
    assert!(report.packages.is_empty());
    assert!(report.repositories.is_empty());
}

#[test]
fn json_parser_drops_malformed_rrid() {
    // `JSONParser::parse` swallows the RRID parse error so a bad id degrades to
    // `None` instead of failing the whole load. One case per failure mode of
    // the RRID grammar (a Contract).
    for rrid in [
        "SUSE:Maintenance:24993",     // MissingComponent — truncated
        "SUSE:Maintenance:24993:abc", // ComponentParse — non-integer review id
        "openSUSE:Maintenance:1:2",   // ComponentParse — wrong project
        "SUSE:boo:1:2",               // ComponentParse — unknown kind
        "SUSE:Maintenance:1:2:3",     // TooManyComponents
        "",                           // MissingComponent — empty
    ] {
        // Seed a good RRID first: against the `None` default the assertion
        // below would pin the *initial state* and pass even for a parser that
        // never touched the field.
        let mut report = empty_report();
        JSONParser::parse_str(&mut report, r#"{"rrid": "SUSE:Maintenance:1:1"}"#)
            .expect("valid json");
        assert!(report.rrid.is_some(), "precondition: seeded a valid rrid");

        let data = format!(r#"{{"rrid": "{rrid}", "packager": "someone@suse.de"}}"#);
        JSONParser::parse_str(&mut report, &data).expect("valid json");

        assert!(
            report.rrid.is_none(),
            "malformed rrid {rrid:?} must be dropped, got {:?}",
            report.rrid
        );
        // The load is lenient, not aborted: neighbouring fields still apply.
        assert_eq!(report.packager, "someone@suse.de", "for rrid {rrid:?}");
    }
}

#[test]
fn json_parser_clears_rrid_when_key_absent() {
    // The other route to `None`: the assignment is unconditional, so a payload
    // with no `rrid` key clears whatever was there. Seeded for the same reason.
    let mut report = empty_report();
    JSONParser::parse_str(&mut report, r#"{"rrid": "SUSE:Maintenance:1:1"}"#).expect("valid json");
    assert!(report.rrid.is_some(), "precondition: seeded a valid rrid");

    JSONParser::parse_str(&mut report, r#"{"packager": "someone@suse.de"}"#).expect("valid json");

    assert!(report.rrid.is_none());
}

/// Golden snapshot of the parsed `metadata.json` envelope.
///
/// `json_parser_parses_golden_fixture` pins individual values; this freezes the
/// whole parsed view, so an envelope→struct mapping regression surfaces as one
/// reviewable diff. `HashMap`/`HashSet` fields render sorted, for determinism.
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
    // No file -> empty map.
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(patchinfo_titles(dir.path()).is_empty());
}

#[test]
fn patchinfo_titles_malformed_is_empty() {
    // Unparseable XML must degrade to an empty map rather than an error.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("patchinfo.xml"), "<patchinfo><issue ")
        .expect("write patchinfo");
    assert!(patchinfo_titles(dir.path()).is_empty());
}

/// #396: a product whose entries all fail to parse must NOT leave an empty
/// sub-map behind — that makes `base.packages` non-empty while the flattened
/// package list is empty, defeating every "anything to install?" guard.
///
/// Key- and separator-agnostic by design: the parser inspects neither, so the
/// odd `_` separator is a deliberate probe of that, not a claim that metadata
/// ships one (`json_parser_parses_slfo_real_fixture` has the real shape).
#[test]
fn json_parser_drops_products_with_no_parsable_entries() {
    let mut results = TestReportBase::new(Config::default());
    JSONParser::parse_str(
        &mut results,
        r#"{"packages": {"6.1": ["afterburn 5.9.0"], "other": ["afterburn _ 5.9.0-1"]}}"#,
    )
    .unwrap();
    assert!(
        !results.packages.contains_key("6.1"),
        "two-token-only product must be dropped: {:?}",
        results.packages
    );
    assert_eq!(results.packages["other"]["afterburn"], "5.9.0-1");
}

/// One- and two-token entries are dropped (loudly); three-plus-token entries
/// keep parsing as first=name, third=version.
///
/// The middle token is *discarded*, so the separator is not significant — the
/// deliberately odd `_` below probes that rather than claiming a wire format.
/// Real metadata ships `=` (`json_parser_parses_slfo_real_fixture`).
#[test]
fn json_parser_drops_short_entries_keeps_three_plus() {
    let mut results = TestReportBase::new(Config::default());
    JSONParser::parse_str(
        &mut results,
        r#"{"packages": {"6.1": [
            "solo",
            "afterburn 5.9.0",
            "afterburn-dracut _ 5.9.0-1",
            "four _ 1.2.3 extra"
        ]}}"#,
    )
    .unwrap();
    let m = &results.packages["6.1"];
    assert_eq!(m.len(), 2, "{m:?}");
    assert_eq!(m["afterburn-dracut"], "5.9.0-1");
    assert_eq!(m["four"], "1.2.3", "trailing tokens tolerated, third wins");
}

/// An empty packages envelope stays an empty map (no placeholder products).
#[test]
fn json_parser_empty_packages_envelope_yields_empty_map() {
    let mut results = TestReportBase::new(Config::default());
    JSONParser::parse_str(&mut results, r#"{"packages": {}}"#).unwrap();
    assert!(results.packages.is_empty());
}

/// #397 — the **real** `SUSE:SLFO:1.1:418286` envelope parses, pinning the
/// observed shape of an SLFO `packages` map: a single `"standard"` key.
///
/// **Only the key is new.** The entry grammar — `=` as the separator, the
/// version as the *third* token — was already pinned against captured data by
/// `json_parser_parses_golden_fixture`, so the version assertions below are
/// belt-and-braces. The key is what makes `packages_for_map`'s single-key
/// branch the live path for SLFO (see
/// `slfo_real_fixture_seeds_packages_for_any_base_version`).
///
/// The key-set assertion cannot detect TeReGen changing shape upstream:
/// production copies the JSON key verbatim, so `results.packages.keys()` is
/// definitionally this checked-in file's own keys. Its value is pinning *this*
/// record — swapping the fixture for a different update, or losing a field in
/// the parser, fails loudly here rather than silently weakening everything
/// downstream. So too the `rrid` and version assertions.
#[test]
fn json_parser_parses_slfo_real_fixture() {
    let mut results = empty_report();
    JSONParser::parse_str(&mut results, SLFO_METADATA_JSON).expect("real SLFO metadata parses");

    // The whole key set rather than a `contains_key`, so the fixture is pinned
    // to exactly what was captured.
    let products: HashSet<&str> = results.packages.keys().map(String::as_str).collect();
    assert_eq!(
        products,
        HashSet::from(["standard"]),
        "the captured SLFO record is keyed \"standard\" and nothing else: {:?}",
        results.packages
    );

    let standard = &results.packages["standard"];
    assert_eq!(standard.len(), 2, "{standard:?}");
    // Versions, not just names: a parser that took the *second* token would
    // still produce both names, with "=" as their version.
    for name in ["afterburn", "afterburn-dracut"] {
        assert_eq!(
            standard[name], "5.10.0.git73.b97f772-99999_stage.1.1",
            "wrong version parsed for {name}"
        );
    }

    // The rest of the envelope comes from the same record, so a fixture swapped
    // for a different update would not quietly keep passing.
    assert_eq!(
        results.rrid.as_ref().map(ToString::to_string).as_deref(),
        Some("SUSE:SLFO:1.1:418286")
    );
}

/// #397, the load-bearing one: the **real** fixture driven through
/// `packages_for_map`, seeding a host whose `base_version` is deliberately
/// *not* a key in the map. Product-agnosticism is the entire point of the
/// single-`"standard"`-key branch, so a coincidentally-matching `base_version`
/// would disarm the test.
///
/// `SL-Micro` / `6.1` comes from the fixture itself, not minted: the captured
/// record's `products` line reads
/// `SL-Micro 6.1 (aarch64, ppc64le, s390x, x86_64)` and its `testplatform` line
/// `base=SL-Micro(major=6,minor=1);…`, so `6.1` is exactly what a host running
/// this update reports. The first test to connect the parser,
/// `packages_for_map` and a real envelope.
#[test]
fn slfo_real_fixture_seeds_packages_for_any_base_version() {
    let mut results = empty_report();
    JSONParser::parse_str(&mut results, SLFO_METADATA_JSON).expect("real SLFO metadata parses");
    assert!(
        !results.packages.contains_key("6.1"),
        "precondition: the base_version must not be a map key, or the \"standard\" \
         branch is not what is under test: {:?}",
        results.packages
    );

    let pkgs = packages_for_map(&results.packages, "6.1");

    // Sorted test-side: `packages_for_map` happens to sort, but leaning on that
    // turns a dropped `sort_by` into a flake instead of a clean failure.
    let mut names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["afterburn", "afterburn-dracut"], "{names:?}");
    // `required` too, not just the names: seeding it `None` leaves the names
    // intact while the before/after version checks lose their baseline.
    for p in &pkgs {
        assert_eq!(
            p.required().map(ToString::to_string).as_deref(),
            Some("5.10.0.git73.b97f772-99999_stage.1.1"),
            "required version must be seeded for {}",
            p.name
        );
    }
}

/// #397 — pins the `map.len() == 1` half of `packages_for_map`'s guard, which
/// the single-key test cannot: with exactly one key,
/// `map.len() == 1 && map.contains_key("standard")` and a weakened
/// `map.contains_key("standard")` behave identically there.
///
/// Hand-built on purpose and **not** a claim about a real shape (the captured
/// records ship `"standard"` alone). It is a guard-shape probe: if such a map
/// ever appeared the base-version branch must win, because `"standard"` only
/// means "product-agnostic" when it is the whole map.
#[test]
fn packages_for_map_second_product_key_disables_standard_branch() {
    let map: HashMap<String, HashMap<String, String>> = HashMap::from([
        (
            "standard".to_owned(),
            HashMap::from([(
                "afterburn".to_owned(),
                "5.10.0.git73.b97f772-99999_stage.1.1".to_owned(),
            )]),
        ),
        (
            "15-SP6".to_owned(),
            HashMap::from([("hplip".to_owned(), "3.26.4-150600.4.12.1".to_owned())]),
        ),
    ]);

    let pkgs = packages_for_map(&map, "15-SP6");

    let mut names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["hplip"],
        "a second product key must fall through to the base-version branch"
    );
    // Versions too: asserting names alone survives a mutation that seeds no
    // `required`, which the base-version branch must do just as `"standard"` does.
    assert_eq!(
        pkgs[0].required().map(ToString::to_string).as_deref(),
        Some("3.26.4-150600.4.12.1"),
        "the base-version branch must seed required too"
    );
}
