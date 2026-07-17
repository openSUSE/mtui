//! The host-system parser: probes a target over SFTP and builds a [`System`].
//!
//! Ported from `mtui/hosts/target/parsers/system.py`. Like upstream, every SFTP
//! read is issued through a **single** batched [`SftpSession`] opened once via
//! [`Connection::sftp_session`] — a host with many product files pays the SFTP
//! channel+subsystem handshake once, not per probe (mtui-rs-0mop.3, restoring
//! upstream's `sftp_session()` shape). The `sftp_session_count == 1` invariant
//! is pinned by the `parse_system_opens_single_sftp_session` oracle.
//!
//! The branch logic — SUSE detection via `/etc/products.d`, the non-SUSE
//! `/etc/os-release` → RHEL-6 fallback chain, baseproduct symlink
//! normalization, dangling/missing-base degradation, the addon loop, the
//! SLES_SAP 12/16 repo workarounds, and the two-location transactional probe —
//! mirrors upstream exactly.

use std::collections::BTreeSet;
use std::path::Path;

use mtui_types::system::{System, SystemProduct};
use tracing::warn;

use super::product;
use crate::connection::{Connection, SftpSession};
use crate::error::{HostError, Result};

const PRODUCTS_DIR: &str = "/etc/products.d";
const BASEPRODUCT_LINK: &str = "/etc/products.d/baseproduct";
const OS_RELEASE: &str = "/etc/os-release";
const TRANSACTIONAL_CONFS: [&str; 2] = [
    "/usr/etc/transactional-update.conf",
    "/etc/transactional-update.conf",
];

/// Parses the system information of a target host over SFTP.
///
/// Returns the [`System`] (base product + addons, with `dangling_base` set when
/// the baseproduct symlink could not be resolved) and a boolean indicating
/// whether the host is a transactional (read-only-root) system.
///
/// # Errors
/// Propagates an unexpected SFTP error (anything other than the "not found"
/// status that drives the SUSE/non-SUSE/dangling branches) or a product-file
/// XML parse error.
pub async fn parse_system(conn: &mut dyn Connection) -> Result<(System, bool)> {
    // Read every probe through one batched SFTP session (upstream
    // `sftp_session()`): the channel+subsystem handshake is paid once for the
    // whole parse instead of per file. `hostname` is captured before the
    // borrow so error/warn context survives while `sftp` holds `&mut conn`.
    let hostname = conn.hostname().to_owned();
    let mut sftp = conn.sftp_session().await?;

    // Step 1: list /etc/products.d to decide SUSE vs non-SUSE.
    let (suse, mut files) = match sftp.listdir(Path::new(PRODUCTS_DIR)).await {
        Ok(entries) => {
            let prod_files: Vec<String> = entries
                .into_iter()
                .filter(|x| x != "qa.prod" && x.ends_with(".prod"))
                .collect();
            (true, prod_files)
        }
        Err(HostError::SftpNotFound { .. }) => (false, Vec::new()),
        Err(e) => return Err(e),
    };

    // Step 2: non-SUSE hosts fall back to /etc/os-release, then to RHEL 6.
    if !suse {
        return parse_non_suse(sftp.as_mut()).await;
    }

    // Step 3: resolve the base product via the baseproduct symlink.
    let basefile = resolve_basefile(sftp.as_mut()).await?;
    if let Some(name) = &basefile {
        files.retain(|f| f != name);
    }

    let mut dangling_base = false;
    let base = match &basefile {
        None => {
            warn!(
                host = %hostname,
                "{PRODUCTS_DIR}/baseproduct is missing or not a symlink"
            );
            dangling_base = true;
            SystemProduct::new("unknown", "", "")
        }
        Some(basefile) => {
            let base_path = format!("{PRODUCTS_DIR}/{basefile}");
            match sftp.open(Path::new(&base_path)).await {
                Ok(bytes) => {
                    let (name, version, arch) = product::parse_product(&bytes)?;
                    SystemProduct::new(name, version, arch)
                }
                Err(_) => {
                    // Dangling symlink: the target product file is gone. Warn
                    // and fall back to a best-effort base from the link name.
                    warn!(
                        host = %hostname,
                        "{BASEPRODUCT_LINK} -> {basefile} is a dangling symlink \
                         (target product file missing)"
                    );
                    dangling_base = true;
                    let stem = basefile.strip_suffix(".prod").unwrap_or(basefile);
                    SystemProduct::new(stem, "", "")
                }
            }
        }
    };

    // Step 4: collect the remaining product files as addons.
    let mut addons: BTreeSet<SystemProduct> = BTreeSet::new();
    for x in &files {
        let path = format!("{PRODUCTS_DIR}/{x}");
        let bytes = sftp.open(Path::new(&path)).await?;
        let (name, version, arch) = product::parse_product(&bytes)?;
        addons.insert(SystemProduct::new(name, version, arch));
    }

    // SLES4SAP on sle12 also carries SLES + sle-ha repos.
    if base.name == "SLES_SAP" && base.version.starts_with("12") {
        addons.insert(SystemProduct::new("SLES", &base.version, &base.arch));
        addons.insert(SystemProduct::new("sle-ha", &base.version, &base.arch));
    }
    // SLES_SAP 16.0x product/repository mismatch workaround.
    if base.name == "SLES_SAP" && base.version.starts_with("16") {
        addons.insert(SystemProduct::new("sle-ha", &base.version, &base.arch));
    }

    // Step 5: transactional-update.conf may live in /usr/etc (newer) or /etc
    // (older). Probe both so the older layout is not misdetected.
    let mut transactional = false;
    for conf in TRANSACTIONAL_CONFS {
        match sftp.open(Path::new(conf)).await {
            Ok(_) => {
                transactional = true;
                tracing::info!(host = %hostname, "host is a transactional system");
                break;
            }
            Err(HostError::SftpNotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }

    Ok((System::new(base, addons, dangling_base), transactional))
}

/// Handles the non-SUSE path: `/etc/os-release` → parsed product, or RHEL 6 when
/// that file is absent.
async fn parse_non_suse(sftp: &mut dyn SftpSession) -> Result<(System, bool)> {
    match sftp.open(Path::new(OS_RELEASE)).await {
        Ok(bytes) => {
            let (name, version, arch) = product::parse_os_release(&bytes)?;
            Ok((
                System::new(
                    SystemProduct::new(name, version, arch),
                    BTreeSet::new(),
                    false,
                ),
                false,
            ))
        }
        Err(HostError::SftpNotFound { .. }) => {
            // TODO: old RH systems have only /etc/redhat-release.
            Ok((
                System::new(
                    SystemProduct::new("rhel", "6", "x86_64"),
                    BTreeSet::new(),
                    false,
                ),
                false,
            ))
        }
        Err(e) => Err(e),
    }
}

/// Reads and normalizes the `baseproduct` symlink target to a bare basename.
///
/// The target may be absolute (`/etc/products.d/SLES.prod`) or relative
/// (`SLES.prod`); both normalize to the basename so the product file is looked
/// up under `/etc/products.d`. A missing/empty target yields `None` (treated as
/// a dangling base upstream).
async fn resolve_basefile(sftp: &mut dyn SftpSession) -> Result<Option<String>> {
    let target = match sftp.readlink(Path::new(BASEPRODUCT_LINK)).await {
        Ok(t) => t,
        Err(HostError::SftpNotFound { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    if target.is_empty() {
        return Ok(None);
    }
    let basename = Path::new(&target)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(target);
    Ok(Some(basename))
}

#[cfg(test)]
mod tests {
    use crate::connection::MockConnection;

    /// A SLES 15-SP5 host carrying a base product plus **two** addon product
    /// files — exercises the multi-read addon loop so the single-session
    /// batching is meaningfully tested (upstream `sftp_session()` shape).
    fn sles_with_two_addons() -> MockConnection {
        let base = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let ha = br#"<product><name>sle-ha</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let we = br#"<product><name>sle-we</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        MockConnection::new("sles.example")
            .with_listing(
                "/etc/products.d",
                ["SLES.prod", "sle-ha.prod", "sle-we.prod"],
            )
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", base.to_vec())
            .with_file("/etc/products.d/sle-ha.prod", ha.to_vec())
            .with_file("/etc/products.d/sle-we.prod", we.to_vec())
    }

    #[tokio::test]
    async fn parse_system_opens_single_sftp_session() {
        // mtui-rs-0mop.3 handshake-count oracle: a host with a base + 2 addons
        // (listdir + readlink + 3 opens + 2 transactional probes = 7 reads)
        // must open exactly ONE batched SFTP session, not one per read.
        let mut conn = sles_with_two_addons();
        let (_system, _transactional) = super::parse_system(&mut conn).await.expect("parse ok");
        assert_eq!(
            conn.sftp_session_count(),
            1,
            "parse_system must batch every SFTP read through a single session"
        );
    }

    #[tokio::test]
    async fn parse_system_output_is_unchanged_by_batching() {
        // Isomorphism proof: the parsed System (base + addons + dangling flag)
        // and the transactional flag are byte-for-byte the expected values —
        // routing reads through the batched session changed no output.
        let mut conn = sles_with_two_addons();
        let (system, transactional) = super::parse_system(&mut conn).await.expect("parse ok");

        assert_eq!(system.get_base().name, "SLES");
        assert_eq!(system.get_base().version, "15-SP5");
        assert_eq!(system.get_base().arch, "x86_64");
        assert!(!system.dangling_base);
        assert!(!transactional);

        // Full structural snapshot via Debug (System is not PartialEq): pins
        // the exact base + sorted addon set the golden fixture must yield.
        assert_eq!(
            format!("{:?}", system.get_addons()),
            r#"{SystemProduct { name: "sle-ha", version: "15-SP5", arch: "x86_64" }, SystemProduct { name: "sle-we", version: "15-SP5", arch: "x86_64" }}"#
        );
    }

    #[tokio::test]
    async fn parse_system_reconnects_at_session_entry_when_link_down() {
        // A dropped link reconnects once at session open (batched path mirrors
        // the per-op `sftp()` reconnect-if-inactive), then parses normally.
        let mut conn = sles_with_two_addons().inactive();
        let (system, _t) = super::parse_system(&mut conn)
            .await
            .expect("parse ok after reconnect");
        assert_eq!(conn.reconnect_count(), 1);
        assert_eq!(system.get_base().name, "SLES");
    }

    #[tokio::test]
    async fn parse_system_surfaces_reconnect_failure_at_entry() {
        // If the link is down and reconnect fails, session open fails closed —
        // no partial parse.
        let mut conn = sles_with_two_addons().inactive().failing_reconnect();
        let err = super::parse_system(&mut conn)
            .await
            .expect_err("must fail when the session cannot open");
        assert!(matches!(
            err,
            crate::error::HostError::ReconnectFailed { .. }
        ));
    }

    #[tokio::test]
    async fn parse_system_propagates_mid_session_error_without_retry() {
        // A generic (non not-found) open failure on a product file propagates
        // out of the batch unchanged — no auto-retry inside the session.
        let mut conn = sles_with_two_addons().with_open_error("/etc/products.d/sle-ha.prod");
        let err = super::parse_system(&mut conn)
            .await
            .expect_err("mid-session open error must propagate");
        assert!(matches!(err, crate::error::HostError::Sftp { .. }));
        // Still only one session was opened (the failure was mid-batch).
        assert_eq!(conn.sftp_session_count(), 1);
    }
}
