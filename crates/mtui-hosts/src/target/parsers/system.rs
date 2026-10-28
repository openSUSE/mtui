//! The host-system parser: probes a target over SFTP and builds a [`System`].
//!
//! Every SFTP
//! read is issued through a **single** batched [`SftpSession`] opened once via
//! [`Connection::sftp_session`] — a host with many product files pays the SFTP
//! channel+subsystem handshake once, not per probe. The
//! `sftp_session_count == 1` invariant
//! is pinned by the `parse_system_opens_single_sftp_session` oracle.
//!
//! The branch logic — SUSE detection via `/etc/products.d`, the non-SUSE
//! `/etc/os-release` → RHEL-6 fallback chain, baseproduct symlink
//! normalization, dangling/missing-base degradation, the addon loop, the
//! SLES_SAP 12/16 repo workarounds, and the two-location transactional probe.

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
/// The mount table of the SFTP server process. It shows the host's `/` when
/// the server runs in the host's mount namespace, as a stock `sftp-server`
/// does; under a `ChrootDirectory` or another namespace it may not, and the
/// parse then falls back as described in [`parse_system`].
const MOUNTS: &str = "/proc/self/mounts";

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
    // Read every probe through one batched SFTP session: the
    // channel+subsystem handshake is paid once for the
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
    let mut has_conf = false;
    for conf in TRANSACTIONAL_CONFS {
        match sftp.open(Path::new(conf)).await {
            Ok(_) => {
                has_conf = true;
                break;
            }
            Err(HostError::SftpNotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }

    // The config file alone is not enough: a read-write host can carry it too
    // (#686). Only a read-only `/` makes the host transactional.
    //
    // When the mount table gives no answer (the file is missing, or it has no
    // `/` entry), keep the config-file answer from before #686. That is the
    // safer mistake: a transactional host taken for a standard one would run
    // zypper on its read-only root, while the reverse only refuses commands.
    // Any other error reading it is propagated, like the config probes above,
    // so a transport or permission failure is not taken for either answer.
    let transactional = has_conf
        && match sftp.open(Path::new(MOUNTS)).await {
            Ok(bytes) => match root_is_read_only(&bytes) {
                Some(read_only) => read_only,
                None => {
                    warn!(host = %hostname, "no / entry in {MOUNTS}; keeping the transactional-update.conf answer");
                    true
                }
            },
            Err(HostError::SftpNotFound { .. }) => {
                warn!(host = %hostname, "{MOUNTS} not found; keeping the transactional-update.conf answer");
                true
            }
            Err(e) => return Err(e),
        };
    if transactional {
        tracing::info!(host = %hostname, "host is a transactional system");
    } else if has_conf {
        tracing::info!(
            host = %hostname,
            "host has transactional-update.conf but a read-write root; not transactional"
        );
    }

    Ok((System::new(base, addons, dangling_base), transactional))
}

/// Whether `/` is mounted read-only according to a mount table in the format
/// of `/proc/self/mounts`, or `None` when the table has no `/` entry.
///
/// The last `/` entry wins, as it is the one mounted on top. The options field
/// is matched by whole option, so `errors=remount-ro` does not count as `ro`.
fn root_is_read_only(mounts: &[u8]) -> Option<bool> {
    String::from_utf8_lossy(mounts)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (_device, mountpoint, _fstype, options) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            (mountpoint == "/").then(|| options.split(',').any(|o| o == "ro"))
        })
        .next_back()
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
/// a dangling base).
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
    /// batching is meaningfully tested.
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
        // Handshake-count oracle: a host with a base + 2 addons
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
    async fn parse_system_reads_the_mount_table_in_the_same_session() {
        // With transactional-update.conf present the mount table is read too
        // (listdir + readlink + 3 opens + 1 conf probe + mounts = 7 reads),
        // still through the one batched session.
        let mut conn = sles_with_two_addons()
            .with_file("/usr/etc/transactional-update.conf", b"".to_vec())
            .with_file(
                "/proc/self/mounts",
                b"/dev/vda2 / btrfs rw,relatime 0 0\n".to_vec(),
            );
        let (_system, transactional) = super::parse_system(&mut conn).await.expect("parse ok");
        assert!(!transactional);
        assert_eq!(conn.sftp_session_count(), 1);
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

    #[test]
    fn root_is_read_only_reads_the_last_root_entry_by_whole_option() {
        use super::root_is_read_only;
        let ro = b"proc /proc proc rw,nosuid 0 0\n/dev/vda3 / btrfs ro,relatime,subvol=/@/.snapshots/5/snapshot 0 0\n";
        assert_eq!(root_is_read_only(ro), Some(true));
        // #686: SLES 16 with a read-write root.
        let rw = b"/dev/vda2 / btrfs rw,relatime,subvol=/@/.snapshots/413/snapshot 0 0\n";
        assert_eq!(root_is_read_only(rw), Some(false));
        // `errors=remount-ro` is not the `ro` option.
        let ext4 = b"/dev/sda1 / ext4 rw,relatime,errors=remount-ro 0 0\n";
        assert_eq!(root_is_read_only(ext4), Some(false));
        // The entry mounted on top wins.
        let over = b"rootfs / rootfs rw 0 0\n/dev/vda3 / btrfs ro 0 0\n";
        assert_eq!(root_is_read_only(over), Some(true));
        let rw_on_top = b"/dev/vda3 / btrfs ro 0 0\n/dev/vda2 / btrfs rw 0 0\n";
        assert_eq!(root_is_read_only(rw_on_top), Some(false));
        // `/var` being read-only says nothing about `/`.
        let no_root = b"/dev/vda4 /var btrfs ro 0 0\n";
        assert_eq!(root_is_read_only(no_root), None);
        assert_eq!(root_is_read_only(b""), None);
    }
}
