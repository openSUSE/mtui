//! Normalizers for miscellaneous product families.
//!
//! Ported from `mtui/test_reports/products/misc.py`. Upstream mutates a
//! `[name, version, arch]` list in place and returns it; the idiomatic Rust
//! port takes a [`SystemProduct`] by value, rewrites its fields, and returns it.

use mtui_types::SystemProduct;

/// Normalizes SES (SUSE Enterprise Storage) product information.
///
/// Rewrites the name to `ses` unconditionally.
#[must_use]
pub fn normalize_ses(mut x: SystemProduct) -> SystemProduct {
    x.name = "ses".to_string();
    x
}

/// Normalizes SLES-RT (Real Time) product information.
///
/// Rewrites the name to `SUSE-Linux-Enterprise-RT` unconditionally.
#[must_use]
pub fn normalize_rt(mut x: SystemProduct) -> SystemProduct {
    x.name = "SUSE-Linux-Enterprise-RT".to_string();
    x
}

/// Normalizes SUSE Manager product information.
///
/// Only `SLE-Manager-Tools` is rewritten (to `sle-manager-tools`); every other
/// name (including `SUSE-Manager-Server`) passes through unchanged.
#[must_use]
pub fn normalize_manager(mut x: SystemProduct) -> SystemProduct {
    if x.name == "SLE-Manager-Tools" {
        x.name = "sle-manager-tools".to_string();
    }
    x
}

/// Normalizes openSUSE Leap product information.
///
/// Note the deliberate field shift mirrored from upstream: the name becomes
/// `leap`, the version is replaced by the old `arch` value, and the arch is
/// reset to `x86_64`. This is intentional, not a bug.
#[must_use]
pub fn normalize_osle(mut x: SystemProduct) -> SystemProduct {
    x.name = "leap".to_string();
    x.version = x.arch;
    x.arch = "x86_64".to_string();
    x
}
