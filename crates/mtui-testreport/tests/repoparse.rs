//! Ported from the `*repoparse` cases in upstream
//! `tests/test_metadata_parsers.py` (originally `tests/test_repoparse.py`).
//!
//! Golden checks that the repository-URL derivation reproduces upstream's exact
//! output strings for the SL-report dispatch targets (`parse_product`,
//! `slrepoparse`, `gitrepoparse`, `reporepoparse`). `obsrepoparse` is deferred
//! with the OBS report.

use mtui_testreport::{gitrepoparse, parse_product, reporepoparse, slrepoparse};
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
