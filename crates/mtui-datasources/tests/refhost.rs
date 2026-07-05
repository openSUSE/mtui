//! Integration coverage for the refhost search engine against the golden
//! `refhosts.yml` fixture (ported from upstream `tests/fixtures/refhosts.yml`).
//!
//! These exercise the full `Refhosts::from_path` → `search` / `host_by_name`
//! path over the real fixture, complementing the unit tests that use in-memory
//! pools. Everything runs offline.

use std::path::Path;

use mtui_datasources::error::RefhostError;
use mtui_datasources::{Attributes, Refhosts};

fn fixture() -> Refhosts {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/refhosts.yml");
    Refhosts::from_path(&path).expect("golden refhosts.yml loads")
}

#[test]
fn merges_all_former_location_groups() {
    let rh = fixture();
    let names: std::collections::BTreeSet<&str> =
        rh.hosts().iter().map(|h| h.name.as_str()).collect();
    assert!(names.contains("host-default-x86"));
    assert!(names.contains("host-nbg-only-here"));
    // default (3) + nuremberg (2), no dupes.
    assert_eq!(rh.hosts().len(), 5);
}

#[test]
fn search_finds_hosts_across_former_locations() {
    let rh = fixture();
    let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
    let found: std::collections::BTreeSet<String> = rh.search(&attrs).into_iter().collect();
    assert_eq!(
        found,
        ["host-default-x86", "host-nbg-x86"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}

#[test]
fn search_addon_filter_narrows_to_sdk_host() {
    let rh = fixture();
    let attrs = Attributes::from_testplatform(
        "base=sles(major=15,minor=5);arch=[x86_64];addon=sdk(major=15,minor=5)",
    );
    assert_eq!(rh.search(&attrs), ["host-default-x86"]);
}

#[test]
fn host_by_name_resolves_former_nuremberg_host() {
    let rh = fixture();
    let host = rh.host_by_name("host-nbg-only-here").expect("host present");
    assert_eq!(host.arch, "ppc64le");
    assert!(rh.host_by_name("no-such-host").is_none());
}

#[test]
fn missing_file_is_io_error() {
    let err = Refhosts::from_path(Path::new("/no/such/refhosts.yml")).unwrap_err();
    assert!(matches!(err, RefhostError::Io { .. }));
}

#[test]
fn malformed_document_is_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.yml");
    std::fs::write(&path, "not: valid: yaml: at all: [").unwrap();
    let err = Refhosts::from_path(&path).unwrap_err();
    assert!(matches!(err, RefhostError::Parse(_)));
}
