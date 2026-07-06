//! Product-identity normalizers.
//!
//! Ported from `mtui/test_reports/products/` (`__init__.py`, `misc.py`,
//! `sle11.py`, `sle12.py`, `sle15.py`). These are pure, I/O-free functions that
//! canonicalize SUSE product identity so downstream repo parsers
//! (`obsrepoparse`/`reporepoparse`, landing in a later task) can key on a
//! stable `(name, version, arch)` tuple.
//!
//! ## Deviation from upstream
//!
//! Upstream mutates a `[name, version, arch]` list in place and returns the
//! enclosing container (accessed as `x[0]`). The idiomatic Rust port operates
//! directly on a [`SystemProduct`] by value: each function takes one, rewrites
//! its fields, and returns it. The dispatch ordering and every branch's string
//! rewrite are preserved verbatim.

pub mod misc;
pub mod sle11;
pub mod sle12;
pub mod sle15;

use mtui_types::SystemProduct;

pub use misc::{normalize_manager, normalize_osle, normalize_rt, normalize_ses};
pub use sle11::normalize_sle11;
pub use sle12::normalize_sle12;
pub use sle15::normalize_sle15;

/// Canonicalizes product identity for version 16 products.
///
/// `SLES-SAP` becomes `SLES_SAP` and `SLES-HA` becomes `sle-ha`; every other
/// product passes through unchanged.
#[must_use]
pub fn normalize_16(mut x: SystemProduct) -> SystemProduct {
    if x.name == "SLES-SAP" {
        x.name = "SLES_SAP".to_string();
        return x;
    }
    if x.name == "SLES-HA" {
        x.name = "sle-ha".to_string();
        return x;
    }
    x
}

/// Dispatches a product to its family-specific normalizer.
///
/// The routing order is significant and mirrors upstream exactly:
/// `SLE-RT` (by name) is matched before any version-based comparison, then the
/// `11`/`12`/`15` version prefixes, then `Storage`, then SUSE Manager /
/// SLE Manager Tools (by name substring), then openSUSE Leap (by version
/// substring). Anything unmatched is returned unchanged.
#[must_use]
pub fn normalize(x: SystemProduct) -> SystemProduct {
    // SLE-RT must precede the version-based comparisons.
    if x.name == "SLE-RT" {
        return normalize_rt(x);
    }
    if x.version.starts_with("11") {
        return normalize_sle11(x);
    }
    if x.version.starts_with("12") {
        return normalize_sle12(x);
    }
    if x.version.starts_with("15") {
        return normalize_sle15(x);
    }
    if x.name == "Storage" {
        return normalize_ses(x);
    }
    if x.name.contains("SUSE-Manager") || x.name.contains("SLE-Manager-Tools") {
        return normalize_manager(x);
    }
    if x.version.contains("openSUSE-SLE") {
        return normalize_osle(x);
    }
    // Corner cases: return unchanged.
    x
}
