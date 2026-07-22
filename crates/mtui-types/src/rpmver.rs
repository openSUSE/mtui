//! RPM version comparison, ported from `mtui/types/rpmver.py::RPMVersion`.
//!
//! Upstream delegates the actual comparison to the C `rpm` library's
//! `rpm.labelCompare(("1", ver, rel), …)`. To preserve the mtui
//! single-static-binary / no-runtime-deps contract, this port uses the
//! pure-Rust [`sandogasa_rpmvercmp`] crate, which reimplements the canonical
//! `rpmvercmp` algorithm (segment/tilde/caret handling). It is verified against
//! every upstream `test_rpm_version` golden vector (see `tests/rpmver.rs`).
//!
//! ## Parsing rules (mirrored verbatim from upstream `__init__`)
//! - An empty string is rejected with [`RpmVersionParseError::Empty`] — upstream
//!   raised a bare `ValueError`; the Rust port makes construction fallible.
//! - The seven SLE-12-era architecture suffixes (`.noarch`, `.x86_64`, …) are
//!   stripped wherever they appear, because refhost queriers occasionally append
//!   the arch to the version.
//! - The string is split into `(ver, rel)` on the **last** `-`; the release field
//!   never contains a dash, but the version field can (e.g. Debian-style
//!   `upstream-debrev`). With no dash, `rel` defaults to `"0"`.
//!
//! ## Comparison
//! Two versions are compared by `ver` first, then `rel`, each via `rpmvercmp`.
//! The epoch is fixed to `"1"` on both sides, exactly reproducing upstream's
//! `labelCompare(("1", ver, rel), ("1", ver, rel))`.

use std::cmp::Ordering;

use sandogasa_rpmvercmp::rpmvercmp;

use crate::error::RpmVersionParseError;

/// Architecture suffixes that may be appended to a version on SLE 12.
///
/// Mirrors upstream `RPMVersion._arch_suffixes`. Each is stripped (with its
/// leading `.`) wherever it occurs in the raw version string.
const ARCH_SUFFIXES: [&str; 7] = [
    "noarch", "x86_64", "s390x", "ppc64le", "aarch64", "ia64", "ppc64",
];

/// Holds an RPM version-release string for version arithmetic.
///
/// Construct via [`RPMVersion::parse`]. Ordering follows the RPM `rpmvercmp`
/// algorithm (version compared first, then release), matching upstream
/// `RPMVersion`.
#[derive(Debug, Clone)]
pub struct RPMVersion {
    /// The version component (everything before the last `-`).
    ver: String,
    /// The release component (after the last `-`, or `"0"` when absent).
    rel: String,
}

impl RPMVersion {
    /// Parses an RPM `version[-release]` string.
    ///
    /// Strips architecture suffixes and splits on the last `-`. Returns
    /// [`RpmVersionParseError::Empty`] for an empty input.
    ///
    /// # Errors
    /// Returns [`RpmVersionParseError::Empty`] when `ver` is empty.
    pub fn parse(ver: &str) -> Result<Self, RpmVersionParseError> {
        if ver.is_empty() {
            return Err(RpmVersionParseError::Empty);
        }

        let mut ver = ver.to_owned();
        for suffix in ARCH_SUFFIXES {
            ver = ver.replace(&format!(".{suffix}"), "");
        }

        let (ver, rel) = match ver.rsplit_once('-') {
            Some((v, r)) => (v.to_owned(), r.to_owned()),
            None => (ver, "0".to_owned()),
        };

        Ok(Self { ver, rel })
    }

    /// The version component.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.ver
    }

    /// The release component (`"0"` when the source had no release).
    #[must_use]
    pub fn release(&self) -> &str {
        &self.rel
    }
}

impl std::fmt::Display for RPMVersion {
    /// Renders `ver`, appending `-rel` only when a release is present.
    ///
    /// Mirrors upstream `__str__`: the sentinel release `"0"` is omitted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.ver)?;
        if self.rel != "0" {
            write!(f, "-{}", self.rel)?;
        }
        Ok(())
    }
}

impl Ord for RPMVersion {
    /// Compares by `rpmvercmp(ver)`, falling back to `rpmvercmp(rel)` on a tie.
    ///
    /// Reproduces upstream `labelCompare(("1", ver, rel), ("1", ver, rel))` with
    /// a fixed, equal epoch.
    fn cmp(&self, other: &Self) -> Ordering {
        match rpmvercmp(&self.ver, &other.ver) {
            Ordering::Equal => rpmvercmp(&self.rel, &other.rel),
            non_eq => non_eq,
        }
    }
}

impl PartialOrd for RPMVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RPMVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RPMVersion {}

impl std::hash::Hash for RPMVersion {
    /// Hashes by `(ver, rel)`, mirroring upstream `__hash__`.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ver.hash(state);
        self.rel.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_rejected() {
        assert_eq!(RPMVersion::parse(""), Err(RpmVersionParseError::Empty));
    }

    #[test]
    fn no_dash_defaults_release_to_zero() {
        let v = RPMVersion::parse("2.3").unwrap();
        assert_eq!(v.version(), "2.3");
        assert_eq!(v.release(), "0");
    }

    #[test]
    fn splits_version_and_release_on_dash() {
        let v = RPMVersion::parse("1.2.3-7.3").unwrap();
        assert_eq!(v.version(), "1.2.3");
        assert_eq!(v.release(), "7.3");
    }

    #[test]
    fn splits_on_last_dash_when_version_contains_dashes() {
        let v = RPMVersion::parse("1.2-3-4").unwrap();
        assert_eq!(v.version(), "1.2-3");
        assert_eq!(v.release(), "4");
    }

    #[test]
    fn strips_arch_suffix() {
        let v = RPMVersion::parse("1.2.3.x86_64-7.1").unwrap();
        assert_eq!(v.version(), "1.2.3");
        assert_eq!(v.release(), "7.1");
    }

    #[test]
    fn strips_noarch_suffix() {
        let v = RPMVersion::parse("4.5.noarch").unwrap();
        assert_eq!(v.version(), "4.5");
        assert_eq!(v.release(), "0");
    }

    #[test]
    fn display_omits_zero_release() {
        assert_eq!(RPMVersion::parse("2.3").unwrap().to_string(), "2.3");
    }

    #[test]
    fn display_includes_nonzero_release() {
        assert_eq!(
            RPMVersion::parse("1.2.3-7.3").unwrap().to_string(),
            "1.2.3-7.3"
        );
    }

    #[test]
    fn display_round_trips_version_with_dash() {
        assert_eq!(RPMVersion::parse("1.2-3-4").unwrap().to_string(), "1.2-3-4");
    }

    #[test]
    fn ordering_compares_version_then_release() {
        let lo = RPMVersion::parse("1.2.0-7.20").unwrap();
        let hi = RPMVersion::parse("1.2.0-7.30").unwrap();
        assert!(lo < hi);
        assert!(hi > lo);
    }

    #[test]
    fn equal_versions_hash_equal() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let a = RPMVersion::parse("1.2.0-8.1").unwrap();
        let b = RPMVersion::parse("1.2.0-8.1").unwrap();
        assert_eq!(a, b);

        let mut ha = DefaultHasher::new();
        let mut hb = DefaultHasher::new();
        a.hash(&mut ha);
        b.hash(&mut hb);
        assert_eq!(ha.finish(), hb.finish());
    }
}
