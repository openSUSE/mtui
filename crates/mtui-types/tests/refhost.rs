//! Golden test for the `refhosts.yml` loader against the ported upstream
//! fixture (`tests/fixtures/refhosts.yml`, byte-identical to upstream
//! `mtui/tests/fixtures/refhosts.yml`).
//!
//! This locks the `refhosts.yml` data-format contract: the loader must merge
//! every legacy location group into one flat list and preserve each row's typed
//! shape. The fixture intentionally spreads five uniquely-named hosts across two
//! former locations (`default:`, `nuremberg:`) so the merge is exercised.

use mtui_types::load_refhosts;
use mtui_types::version::{Version, VersionField};

const FIXTURE: &str = include_str!("fixtures/refhosts.yml");

#[test]
fn golden_fixture_merges_all_location_groups() {
    let hosts = load_refhosts(FIXTURE).expect("fixture must parse");

    // All five hosts from both former location groups are merged, in order.
    let names: Vec<_> = hosts.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "host-default-x86",
            "host-default-aarch64",
            "host-default-noaddon",
            "host-nbg-x86",
            "host-nbg-only-here",
        ]
    );
}

#[test]
fn golden_fixture_preserves_numeric_and_text_minors_and_addons() {
    let hosts = load_refhosts(FIXTURE).expect("fixture must parse");

    let x86 = hosts.iter().find(|h| h.name == "host-default-x86").unwrap();
    assert_eq!(x86.arch, "x86_64");
    assert_eq!(x86.product.name, "sles");
    assert_eq!(
        x86.product.version,
        Some(Version::new(15u64, Some(VersionField::Num(5))))
    );
    assert_eq!(x86.addons.len(), 1);
    assert_eq!(x86.addons[0].name, "sdk");

    // Text minor (`sp4`) is preserved distinctly from a numeric minor.
    let noaddon = hosts
        .iter()
        .find(|h| h.name == "host-default-noaddon")
        .unwrap();
    assert_eq!(
        noaddon.product.version,
        Some(Version::new(
            12u64,
            Some(VersionField::Text("sp4".to_owned()))
        ))
    );
    assert!(noaddon.addons.is_empty());

    // A host present only in the `nuremberg:` group survives the merge.
    let only = hosts
        .iter()
        .find(|h| h.name == "host-nbg-only-here")
        .unwrap();
    assert_eq!(only.arch, "ppc64le");
}

#[test]
fn golden_fixture_snapshot() {
    let hosts = load_refhosts(FIXTURE).expect("fixture must parse");
    insta::assert_debug_snapshot!(hosts);
}
