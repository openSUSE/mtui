//! Port of upstream `tests/test_products_sle.py`.
//!
//! Covers the per-family normalizers, `normalize_16`, and the `normalize`
//! dispatcher. Upstream's dispatch tests use `mock.patch` to assert *which*
//! helper ran; since Rust cannot monkeypatch, the dispatch cases here assert on
//! the observable transformed output of each branch, which uniquely identifies
//! the branch taken.

use mtui_testreport::products::{
    normalize, normalize_16, normalize_manager, normalize_osle, normalize_rt, normalize_ses,
    normalize_sle11, normalize_sle12, normalize_sle15,
};
use mtui_types::SystemProduct;

fn p(name: &str, version: &str, arch: &str) -> SystemProduct {
    SystemProduct::new(name, version, arch)
}

// ---------------------------------------------------------------------------
// Per-family normalizers: assert the rewritten name (and version where upstream
// strips a suffix).
// ---------------------------------------------------------------------------

#[test]
fn sle11_branches() {
    assert_eq!(
        normalize_sle11(p("SLE-SDK", "11", "x86_64")).name,
        "sle-sdk"
    );
    assert_eq!(
        normalize_sle11(p("SLE-SAP-AIO", "11", "x86_64")).name,
        "SUSE_SLES_SAP"
    );

    let out = normalize_sle11(p("SLE-SERVER", "11-LTSS", "x86_64"));
    assert_eq!(out.name, "SUSE_SLES");
    assert_eq!(out.version, "11");

    assert_eq!(
        normalize_sle11(p("SLE-SMT", "11", "x86_64")).name,
        "sle-smt"
    );
    assert_eq!(
        normalize_sle11(p("SLE-HAE", "11", "x86_64")).name,
        "sle-hae"
    );

    let out = normalize_sle11(p("SLES-SAP", "11-CORE", "x86_64"));
    assert_eq!(out.name, "SUSE_SLES_LTSS-EXTREME-CORE");

    let out = normalize_sle11(p("SLES-SAP", "11-TERADATA", "x86_64"));
    assert_eq!(out.name, "teradata");
    assert_eq!(out.version, "11");

    let out = normalize_sle11(p("SLES-SAP", "11-SECURITY", "x86_64"));
    assert_eq!(out.name, "security");
    assert_eq!(out.version, "11");

    let out = normalize_sle11(p("SLES-SAP", "11-PUBCLOUD", "x86_64"));
    assert_eq!(out.name, "sle-module-pubcloud");
    assert_eq!(out.version, "11");
}

#[test]
fn sle12_branches() {
    let out = normalize_sle12(p("SLE-SERVER", "12-LTSS-Extended-Security", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS-Extended-Security");
    assert_eq!(out.version, "12");

    let out = normalize_sle12(p("SLE-SERVER", "12-LTSS-ERICSSON", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS-ERICSSON");
    assert_eq!(out.version, "12");

    let out = normalize_sle12(p("SLE-SERVER", "12-LTSS-SAP", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS-SAP");
    assert_eq!(out.version, "12");

    let out = normalize_sle12(p("SLE-SERVER", "12-LTSS-TERADATA", "x86_64"));
    assert_eq!(out.name, "SLES_LTSS_TERADATA");
    assert_eq!(out.version, "12");

    let out = normalize_sle12(p("SLE-SERVER", "12-LTSS", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS");
    assert_eq!(out.version, "12");

    let out = normalize_sle12(p("SLE-SERVER", "12-TERADATA", "x86_64"));
    assert_eq!(out.name, "SLES_TERADATA");
    assert_eq!(out.version, "12");

    assert_eq!(
        normalize_sle12(p("SLE-SERVER", "12", "x86_64")).name,
        "SLES"
    );
    assert_eq!(
        normalize_sle12(p("SLE-DESKTOP", "12", "x86_64")).name,
        "SLED"
    );
    assert_eq!(
        normalize_sle12(p("SLE-RPI", "12", "x86_64")).name,
        "SLES_RPI"
    );
    assert_eq!(
        normalize_sle12(p("SLE-SAP", "12", "x86_64")).name,
        "SLES_SAP"
    );
    assert_eq!(
        normalize_sle12(p("SLE-Module-Web", "12", "x86_64")).name,
        "sle-module-web"
    );
}

#[test]
fn sle15_branches() {
    let out = normalize_sle15(p("SLE-Product-SLES", "15-LTSS-TERADATA", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS-TERADATA");
    assert_eq!(out.version, "15");

    // Triple-pin: `LTSS-ERICSSON` must match its combined branch before bare
    // `LTSS`, stripping the whole suffix (upstream sle15.py; ec4174cb/0bdd17b4).
    let out = normalize_sle15(p("SLE-Product-SLES", "15-SP4-LTSS-ERICSSON", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS-ERICSSON");
    assert_eq!(out.version, "15-SP4");
    assert_eq!(out.arch, "x86_64");

    let out = normalize_sle15(p("SLE-Product-SLES", "15-LTSS", "x86_64"));
    assert_eq!(out.name, "SLES-LTSS");
    assert_eq!(out.version, "15");

    let out = normalize_sle15(p("SLE-Product-SLES", "15-ERICSSON", "x86_64"));
    assert_eq!(out.name, "ERICSSON");
    assert_eq!(out.version, "15");

    let out = normalize_sle15(p("SLE-Product-SLES", "15-TERADATA", "x86_64"));
    assert_eq!(out.name, "SLES_TERADATA");
    assert_eq!(out.version, "15");

    assert_eq!(
        normalize_sle15(p("SLE-Product-SLES", "15", "x86_64")).name,
        "SLES"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-SLED", "15", "x86_64")).name,
        "SLED"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-WE", "15", "x86_64")).name,
        "sle-we"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-HA", "15", "x86_64")).name,
        "sle-ha"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-HPC", "15", "x86_64")).name,
        "SLE_HPC"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-SLES_SAP", "15", "x86_64")).name,
        "SLES_SAP"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Product-RT", "15", "x86_64")).name,
        "SLE_RT"
    );
    assert_eq!(
        normalize_sle15(p("SLE-Module-Web", "15", "x86_64")).name,
        "sle-module-web"
    );
}

#[test]
fn misc_branches() {
    assert_eq!(normalize_ses(p("Storage", "7", "x86_64")).name, "ses");
    assert_eq!(
        normalize_rt(p("SLE-RT", "12", "x86_64")).name,
        "SUSE-Linux-Enterprise-RT"
    );
    assert_eq!(
        normalize_manager(p("SLE-Manager-Tools", "12", "x86_64")).name,
        "sle-manager-tools"
    );
    // Non-`SLE-Manager-Tools` names pass through unchanged.
    assert_eq!(
        normalize_manager(p("SUSE-Manager-Server", "4.1", "x86_64")).name,
        "SUSE-Manager-Server"
    );
}

#[test]
fn osle_field_shift() {
    // The deliberate field shift: name -> "leap", version <- old arch,
    // arch -> "x86_64".
    let out = normalize_osle(p("openSUSE-SLE", "15.4", "openSUSE-Leap-15.4"));
    assert_eq!(out.name, "leap");
    assert_eq!(out.version, "openSUSE-Leap-15.4");
    assert_eq!(out.arch, "x86_64");
}

// ---------------------------------------------------------------------------
// normalize_16
// ---------------------------------------------------------------------------

#[test]
fn normalize_16_sles_sap_rewrites_name() {
    assert_eq!(normalize_16(p("SLES-SAP", "16", "x86_64")).name, "SLES_SAP");
}

#[test]
fn normalize_16_sles_ha_rewrites_name() {
    assert_eq!(normalize_16(p("SLES-HA", "16", "x86_64")).name, "sle-ha");
}

#[test]
fn normalize_16_passthrough_unchanged() {
    let input = p("FooBar", "16", "x86_64");
    assert_eq!(normalize_16(input.clone()), input);
}

// ---------------------------------------------------------------------------
// Dispatcher: assert observable output per branch (no monkeypatching).
// ---------------------------------------------------------------------------

#[test]
fn dispatch_sle_rt() {
    // SLE-RT routes to normalize_rt before any version comparison.
    assert_eq!(
        normalize(p("SLE-RT", "12", "x86_64")).name,
        "SUSE-Linux-Enterprise-RT"
    );
}

#[test]
fn dispatch_sle11_prefix() {
    assert_eq!(normalize(p("SLE-SDK", "11", "x86_64")).name, "sle-sdk");
}

#[test]
fn dispatch_sle12_prefix() {
    assert_eq!(normalize(p("SLE-SERVER", "12", "x86_64")).name, "SLES");
}

#[test]
fn dispatch_sle15_prefix() {
    assert_eq!(
        normalize(p("SLE-Product-SLES", "15", "x86_64")).name,
        "SLES"
    );
}

#[test]
fn dispatch_storage() {
    assert_eq!(normalize(p("Storage", "7", "x86_64")).name, "ses");
}

#[test]
fn dispatch_manager() {
    // SUSE-Manager-Server matches the manager branch (by name substring); its
    // version "4.1" does not hit the 11/12/15 prefixes.
    assert_eq!(
        normalize(p("SUSE-Manager-Server", "4.1", "x86_64")).name,
        "SUSE-Manager-Server"
    );
    // SLE-Manager-Tools also routes to the manager branch and transforms
    // observably, distinguishing it from the final passthrough.
    assert_eq!(
        normalize(p("SLE-Manager-Tools", "4.1", "x86_64")).name,
        "sle-manager-tools"
    );
}

#[test]
fn dispatch_osle() {
    // Matches on "openSUSE-SLE" appearing in the *version* field.
    let out = normalize(p("leap", "openSUSE-SLE-15.4", "x86_64"));
    assert_eq!(out.name, "leap");
}

#[test]
fn dispatch_unmatched_returns_input() {
    let input = p("NoMatch", "13", "x86_64");
    assert_eq!(normalize(input.clone()), input);
}
