//! The host-system parser: probes a target over SFTP and builds a [`System`].
//!
//! Ported from `mtui/hosts/target/parsers/system.py`. Upstream batches every
//! SFTP op inside a single `sftp_session()`; this port uses the per-op
//! [`Connection`] SFTP methods instead (the batched-session handshake
//! optimization is not a behavioral contract, so the upstream
//! `sftp_session.call_count == 1` assertion is intentionally not ported).
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
use crate::connection::Connection;
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
    // Step 1: list /etc/products.d to decide SUSE vs non-SUSE.
    let (suse, mut files) = match conn.sftp_listdir(Path::new(PRODUCTS_DIR)).await {
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
        return parse_non_suse(conn).await;
    }

    // Step 3: resolve the base product via the baseproduct symlink.
    let basefile = resolve_basefile(conn).await?;
    if let Some(name) = &basefile {
        files.retain(|f| f != name);
    }

    let hostname = conn.hostname().to_owned();
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
            match conn.sftp_open(Path::new(&base_path)).await {
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
        let bytes = conn.sftp_open(Path::new(&path)).await?;
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
        match conn.sftp_open(Path::new(conf)).await {
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
async fn parse_non_suse(conn: &mut dyn Connection) -> Result<(System, bool)> {
    match conn.sftp_open(Path::new(OS_RELEASE)).await {
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
async fn resolve_basefile(conn: &mut dyn Connection) -> Result<Option<String>> {
    let target = match conn.sftp_readlink(Path::new(BASEPRODUCT_LINK)).await {
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
