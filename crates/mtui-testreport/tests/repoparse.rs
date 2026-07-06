//! Ported from the `*repoparse` cases in upstream
//! `tests/test_metadata_parsers.py` (originally `tests/test_repoparse.py`).
//!
//! Golden checks that the repository-URL derivation reproduces upstream's exact
//! output strings for all `*repoparse` variants (`parse_product`, `slrepoparse`,
//! `gitrepoparse`, `reporepoparse`, `obsrepoparse`).

use std::fs;

use mtui_testreport::{gitrepoparse, obsrepoparse, parse_product, reporepoparse, slrepoparse};
use mtui_types::SystemProduct;

#[test]
fn parse_product_splits_archs() {
    let products = parse_product("SLES 15 (x86_64, aarch64)");
    assert!(products.contains(&SystemProduct::new("SLES", "15", "x86_64")));
    assert!(products.contains(&SystemProduct::new("SLES", "15", "aarch64")));
}

#[test]
fn slrepoparse_builds_images_repo_url() {
    let repos = slrepoparse("https://example.com", &["SLES 15 (x86_64)".to_string()]);
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(
        repos[&product],
        "https://example.com/images/repo/SLES-15-x86_64/"
    );
}

#[test]
fn gitrepoparse_builds_standard_url() {
    let repos = gitrepoparse("https://example.com", &["SLES 15 (x86_64)".to_string()]);
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/standard");
}

#[test]
fn reporepoparse_matches_repo_by_name_version_arch() {
    let repos = reporepoparse(
        &["https://example.com/SLES-15-x86_64/".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/SLES-15-x86_64/");
}

#[test]
fn reporepoparse_drops_products_with_no_matching_repo() {
    // A product whose (name-version-arch) appears in no repo URL is omitted.
    let repos = reporepoparse(
        &["https://example.com/OTHER-1-aarch64/".to_string()],
        &["SLES 15 (x86_64)".to_string()],
    );
    assert!(repos.is_empty());
}

#[test]
fn obsrepoparse_parses_project_xml() {
    // Golden port of upstream `test_obsrepoparse`: the fixture's
    // `SLE-Product-SLES:15:x86_64` releasetarget normalizes to
    // `Product("SLES", "15", "x86_64")`, keyed to `<repo>/<repository name>`.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/obs");
    let repos = obsrepoparse("https://example.com", dir.as_ref());
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos[&product], "https://example.com/SLE-15-x86_64");
}

#[test]
fn obsrepoparse_excludes_debug_repositories() {
    // A `<repository>` whose name contains `DEBUG` is dropped (upstream's
    // `if "DEBUG" not in x.attrib["name"]`), leaving only the update repo.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("project.xml"),
        r#"<project>
  <repository name="SLE-15-x86_64">
    <path repository="update" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
  <repository name="SLE-15-x86_64-DEBUG">
    <path repository="update" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
</project>
"#,
    )
    .unwrap();

    let repos = obsrepoparse("https://example.com", tmp.path());
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[&product], "https://example.com/SLE-15-x86_64");
}

#[test]
fn obsrepoparse_skips_repositories_without_update_path() {
    // A `<repository>` with no `path[@repository='update']` child is not an
    // update target and is omitted, matching upstream's XPath selection.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("project.xml"),
        r#"<project>
  <repository name="SLE-15-x86_64-images">
    <path repository="images" project="SUSE:SLE-15:Update"/>
    <releasetarget project="SLE-Product-SLES:15:x86_64"/>
  </repository>
</project>
"#,
    )
    .unwrap();

    let repos = obsrepoparse("https://example.com", tmp.path());
    assert!(repos.is_empty());
}

#[test]
fn obsrepoparse_missing_project_xml_yields_empty_map() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = obsrepoparse("https://example.com", tmp.path());
    assert!(repos.is_empty());
}
