//! Tests for `.github/scripts/release-notes-base.sh`, which picks the base tag
//! for a release's generated notes.
//!
//! The script lives in the workflow rather than in xtask so the release job
//! need not build this crate, but its ordering is the kind that is wrong in
//! subtle ways — `sort -V` orders `26.4.0` before `26.4.0-rc1` — so it is
//! pinned here, where `cargo test --workspace` already runs.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".github/scripts/release-notes-base.sh")
}

/// Run the script for `current` over `tags`; returns (success, trimmed stdout).
fn base(tags: &[&str], current: &str) -> (bool, String) {
    let mut child = Command::new("bash")
        .arg(script())
        .arg(current)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(tags.join("\n").as_bytes())
        .expect("write tags");
    let out = child.wait_with_output().expect("wait");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

const TAGS: &[&str] = &[
    "26.3.0",
    "26.4.0",
    "26.4.0-rc1",
    "26.4.0-rc2",
    "26.4.1",
    "26.10.0",
];

/// The regression this script exists for: under `sort -V` the base for `26.4.1`
/// is `26.4.0-rc2`, so its notes re-list everything `26.4.0` already shipped.
#[test]
fn a_release_takes_the_previous_release_not_its_own_prereleases() {
    assert_eq!(base(TAGS, "26.4.1"), (true, "26.4.0".to_owned()));
}

/// SemVer §11: a prerelease has lower precedence than the release it precedes.
#[test]
fn a_release_takes_its_last_prerelease_as_base() {
    assert_eq!(base(TAGS, "26.4.0"), (true, "26.4.0-rc2".to_owned()));
    assert_eq!(base(TAGS, "26.4.0-rc2"), (true, "26.4.0-rc1".to_owned()));
    assert_eq!(base(TAGS, "26.4.0-rc1"), (true, "26.3.0".to_owned()));
}

/// Numeric fields compare as numbers: `26.10.0` is newer than `26.4.1`.
#[test]
fn version_fields_compare_numerically_not_lexically() {
    assert_eq!(base(TAGS, "26.10.0"), (true, "26.4.1".to_owned()));
}

/// The oldest tag has no predecessor; generating from the repository root is
/// then correct, so this is an empty success rather than a failure.
#[test]
fn the_first_release_has_an_empty_base() {
    assert_eq!(base(TAGS, "26.3.0"), (true, String::new()));
}

/// A tag absent from the set must fail rather than silently pick a neighbour.
#[test]
fn an_absent_release_tag_is_an_error() {
    let (ok, out) = base(TAGS, "26.9.9");
    assert!(
        !ok,
        "expected failure for an absent tag, got stdout {out:?}"
    );
}

/// A malformed tag must fail rather than sort somewhere arbitrary.
#[test]
fn a_non_semver_tag_is_an_error() {
    let (ok, _) = base(&["26.3.0", "v26.4.0"], "26.3.0");
    assert!(!ok, "expected failure for the `v`-prefixed tag");
}

/// SemVer §11: numeric prerelease identifiers rank below alphanumeric ones, and
/// a longer identifier set outranks the prefix it extends.
#[test]
fn prerelease_identifiers_follow_semver_precedence() {
    let tags = &["1.0.0-1", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0"];
    assert_eq!(base(tags, "1.0.0-alpha"), (true, "1.0.0-1".to_owned()));
    assert_eq!(
        base(tags, "1.0.0-alpha.1"),
        (true, "1.0.0-alpha".to_owned())
    );
    assert_eq!(base(tags, "1.0.0"), (true, "1.0.0-alpha.1".to_owned()));
}
