//! Integration port of upstream `tests/test_target_parsers.py::TestParseSystem`.
//!
//! Upstream mocks the `product` module and feeds `parse_system` canned
//! `(name, version, arch)` tuples. This port instead drives the *real*
//! `parse_product` over real product XML bytes served by a `MockConnection`, so
//! the whole SUSE/non-SUSE/dangling/transactional branch matrix is exercised
//! end-to-end (a strictly stronger check than the upstream module-mock).

use mtui_hosts::{Connection, MockConnection, parse_system};

/// Builds a minimal `<product>` XML document for the given fields. When
/// `patchlevel` is `Some`, a `<baseversion>`+`<patchlevel>` pair is emitted (so
/// the parser derives `-SP{n}`); when `None`, a plain `<version>` is emitted.
fn prod_xml(name: &str, baseversion: &str, patchlevel: Option<&str>, arch: &str) -> Vec<u8> {
    let version_block = match patchlevel {
        Some(pl) => {
            format!("<baseversion>{baseversion}</baseversion><patchlevel>{pl}</patchlevel>")
        }
        None => format!("<version>{baseversion}</version>"),
    };
    format!("<product><name>{name}</name>{version_block}<arch>{arch}</arch></product>").into_bytes()
}

const PRODUCTS_DIR: &str = "/etc/products.d";
const BASEPRODUCT: &str = "/etc/products.d/baseproduct";

#[tokio::test]
async fn parse_suse_system() {
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLES.prod", "sle-module-basesystem.prod"])
        .with_link(BASEPRODUCT, "SLES.prod")
        .with_file(
            "/etc/products.d/SLES.prod",
            prod_xml("SLES", "15", Some("5"), "x86_64"),
        )
        .with_file(
            "/etc/products.d/sle-module-basesystem.prod",
            prod_xml("sle-module-basesystem", "15", Some("5"), "x86_64"),
        )
        // both transactional-update.conf probes miss -> non-transactional
        .with_missing_dir("/does-not-matter");

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert_eq!(system.get_base().name, "SLES");
    assert_eq!(system.get_base().version, "15-SP5");
    assert!(!transactional);
}

#[tokio::test]
async fn parse_non_suse_falls_back_to_os_release() {
    let mut conn = MockConnection::new("host1")
        .with_missing_dir(PRODUCTS_DIR)
        .with_file(
            "/etc/os-release",
            b"ID=\"ubuntu\"\nVERSION_ID=\"22.04\"\n".to_vec(),
        );

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert_eq!(system.get_base().name, "ubuntu");
    assert_eq!(system.get_base().version, "22.04");
    assert!(!transactional);
}

#[tokio::test]
async fn parse_non_suse_without_os_release_falls_back_to_rhel() {
    let mut conn = MockConnection::new("host1").with_missing_dir(PRODUCTS_DIR);
    // /etc/os-release also absent -> SftpNotFound -> rhel 6 fallback.

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert_eq!(system.get_base().name, "rhel");
    assert_eq!(system.get_base().version, "6");
    assert!(!transactional);
}

#[tokio::test]
async fn parse_transactional_system_usr_etc() {
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SL-Micro.prod", "SL-Micro-Extras.prod"])
        .with_link(BASEPRODUCT, "SL-Micro.prod")
        .with_file(
            "/etc/products.d/SL-Micro.prod",
            prod_xml("SL-Micro", "6.1", None, "x86_64"),
        )
        .with_file(
            "/etc/products.d/SL-Micro-Extras.prod",
            prod_xml("SL-Micro-Extras", "6.1", None, "x86_64"),
        )
        .with_file("/usr/etc/transactional-update.conf", b"".to_vec());

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert_eq!(system.get_base().name, "SL-Micro");
    assert_eq!(system.get_base().version, "6.1");
    assert!(transactional);
}

#[tokio::test]
async fn parse_transactional_system_etc_config() {
    // Older layout: /usr/etc absent, /etc present.
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLE-Micro.prod"])
        .with_link(BASEPRODUCT, "SLE-Micro.prod")
        .with_file(
            "/etc/products.d/SLE-Micro.prod",
            prod_xml("SLE-Micro", "5.5", None, "x86_64"),
        )
        .with_file("/etc/transactional-update.conf", b"".to_vec());

    let (_, transactional) = parse_system(&mut conn).await.expect("parse");
    assert!(transactional);
}

#[tokio::test]
async fn parse_sles_sap_12_adds_sles_and_ha_addons() {
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLES_SAP.prod"])
        .with_link(BASEPRODUCT, "SLES_SAP.prod")
        .with_file(
            "/etc/products.d/SLES_SAP.prod",
            prod_xml("SLES_SAP", "12", Some("5"), "x86_64"),
        );

    let (system, _) = parse_system(&mut conn).await.expect("parse");
    let addons: std::collections::BTreeSet<(String, String)> = system
        .get_addons()
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    assert!(addons.contains(&("SLES".to_owned(), "12-SP5".to_owned())));
    assert!(addons.contains(&("sle-ha".to_owned(), "12-SP5".to_owned())));
}

#[tokio::test]
async fn parse_sles_sap_16_adds_ha_addon() {
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLES_SAP.prod"])
        .with_link(BASEPRODUCT, "SLES_SAP.prod")
        .with_file(
            "/etc/products.d/SLES_SAP.prod",
            prod_xml("SLES_SAP", "16", Some("0"), "x86_64"),
        );

    let (system, _) = parse_system(&mut conn).await.expect("parse");
    let addons: std::collections::BTreeSet<(String, String)> = system
        .get_addons()
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    // patchlevel 0 -> version "16", ha addon carries that version.
    assert!(addons.contains(&("sle-ha".to_owned(), "16".to_owned())));
}

#[tokio::test]
async fn parse_dangling_baseproduct_symlink() {
    // readlink resolves to SLES.prod but opening it raises a generic OSError.
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLES.prod"])
        .with_link(BASEPRODUCT, "SLES.prod")
        .with_open_error("/etc/products.d/SLES.prod");

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert!(system.dangling_base);
    // Best-effort base name from the symlink target ("SLES.prod" -> "SLES").
    assert_eq!(system.get_base().name, "SLES");
    assert_eq!(system.get_base().version, "");
    assert!(!transactional);
}

#[tokio::test]
async fn parse_absolute_baseproduct_symlink() {
    // Absolute symlink target must normalise to the basename, not false-dangle.
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, ["SLES.prod"])
        .with_link(BASEPRODUCT, "/etc/products.d/SLES.prod")
        .with_file(
            "/etc/products.d/SLES.prod",
            prod_xml("SLES", "15", Some("6"), "x86_64"),
        );

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert!(!system.dangling_base);
    assert_eq!(system.get_base().name, "SLES");
    assert_eq!(system.get_base().version, "15-SP6");
    assert!(!transactional);
}

#[tokio::test]
async fn parse_missing_baseproduct_symlink() {
    // Absent/empty baseproduct symlink degrades to "unknown" instead of crashing.
    let mut conn = MockConnection::new("host1")
        .with_listing(PRODUCTS_DIR, [] as [&str; 0])
        .with_link(BASEPRODUCT, "");

    let (system, transactional) = parse_system(&mut conn).await.expect("parse");
    assert!(system.dangling_base);
    assert_eq!(system.get_base().name, "unknown");
    assert!(!transactional);
}

/// Compile-time proof the parser accepts any `&mut dyn Connection`.
#[tokio::test]
async fn parse_system_accepts_dyn_connection() {
    let mut conn: Box<dyn Connection> =
        Box::new(MockConnection::new("h").with_missing_dir(PRODUCTS_DIR));
    let (system, _) = parse_system(conn.as_mut()).await.expect("parse");
    assert_eq!(system.get_base().name, "rhel");
}
