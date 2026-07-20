//! Golden-vector tests for [`RPMVersion`], ported from upstream
//! `mtui/tests/test_rpm_version.py`. These lock the RPM version-comparison
//! Contract: the exact ordering of the fixture pairs (including the tilde-suffix
//! `~` cases), equality, the empty-string rejection, last-dash splitting, and
//! `Display` rendering.

use mtui_types::{RPMVersion, RpmVersionParseError};

fn v(s: &str) -> RPMVersion {
    RPMVersion::parse(s).unwrap_or_else(|e| panic!("expected {s:?} to parse, got {e}"))
}

/// Upstream `test_version_lt`: the lower string sorts before the higher.
#[test]
fn version_lt() {
    let cases = [
        (
            "2014.104.0.0.2svn15878-21.19",
            "2015.104.0.0.2svn15878-21.12",
        ),
        ("1.2.0-7.20", "1.2.0-7.30"),
        // Tilde-suffix ordering: rpmvercmp compares the trailing segments.
        ("0.9~20170329.eb3dfbb", "0.9~20170329.798fdeb"),
    ];
    for (lower, higher) in cases {
        assert!(v(lower) < v(higher), "{lower} should be < {higher}");
    }
}

/// Upstream `test_version_gt`: the higher string sorts after the lower.
#[test]
fn version_gt() {
    let cases = [
        (
            "2014.104.0.0.2svn15878-21.19",
            "2015.104.0.0.2svn15878-21.12",
        ),
        ("1.2.0-7.20", "1.2.0-7.30"),
        ("0.9~20170329.eb3dfbb", "0.9~20170329.798fdeb"),
    ];
    for (lower, higher) in cases {
        assert!(v(higher) > v(lower), "{higher} should be > {lower}");
    }
}

/// Upstream `test_version_eq`: a version compares equal to itself.
#[test]
fn version_eq() {
    for version in ["1.2.0-8.1", "0.8+12.ae4"] {
        assert_eq!(v(version), v(version));
    }
}

/// Upstream `test_version_le`.
#[test]
fn version_le() {
    for (higher, lower) in [("1.2-2", "1.2-2"), ("1.2.3-7.2", "1.2.3-7.2")] {
        assert!(v(lower) <= v(higher));
    }
}

/// Upstream `test_version_ge`.
#[test]
fn version_ge() {
    for (higher, lower) in [("1.2-2", "1.2-2"), ("1.2.3-7.2", "1.2.3-7.2")] {
        assert!(v(higher) >= v(lower));
    }
}

/// Upstream `test_version_ne`.
#[test]
fn version_ne() {
    assert!(v("1-1.1") != v("1-1.2"));
}

/// Upstream `test_version_none`: an empty version string is rejected.
///
/// Python passed `None` and expected `ValueError`; the Rust API is `&str`, so
/// the equivalent invalid input is the empty string, which returns the typed
/// [`RpmVersionParseError::Empty`] rather than panicking.
#[test]
fn version_empty_is_error() {
    assert_eq!(RPMVersion::parse(""), Err(RpmVersionParseError::Empty));
}

/// Upstream `test_version_with_multiple_dashes_splits_on_last`.
#[test]
fn version_with_multiple_dashes_splits_on_last() {
    let ver = v("1.2-3-4");
    assert_eq!(ver.version(), "1.2-3");
    assert_eq!(ver.release(), "4");
    assert_eq!(ver.to_string(), "1.2-3-4");
}

/// Upstream `test_version_str`: `Display` rendering, including the `-0` sentinel
/// elision and the no-release case.
#[test]
fn version_str() {
    for (version, expected) in [
        ("1.2.3-7.3", "1.2.3-7.3"),
        ("2.3", "2.3"),
        ("0.8+1-0", "0.8+1"),
    ] {
        assert_eq!(v(version).to_string(), expected);
    }
}
