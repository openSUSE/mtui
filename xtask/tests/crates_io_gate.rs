//! Tests for `.github/scripts/crates-io-gate.sh`, which decides whether a
//! release tag publishes to crates.io. Only a plain `X.Y.Z` (Z >= 1) tag on the
//! numerically highest `X.Y.x` branch does; everything else must say `false`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".github/scripts/crates-io-gate.sh")
}

/// Run the gate for `tag` over `branches`; returns (success, `publish` value).
fn gate(tag: &str, branches: &[&str]) -> (bool, String) {
    let mut child = Command::new("bash")
        .arg(script())
        .arg(tag)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(branches.join("\n").as_bytes())
        .expect("write branches");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let publish = stdout
        .lines()
        .find_map(|l| l.strip_prefix("publish="))
        .unwrap_or_default()
        .to_owned();
    (out.status.success(), publish)
}

fn publishes(tag: &str, branches: &[&str]) -> bool {
    let (ok, publish) = gate(tag, branches);
    assert!(ok, "gate must not fail for {tag}");
    assert!(publish == "true" || publish == "false", "got {publish:?}");
    publish == "true"
}

#[test]
fn a_patch_release_on_the_stable_branch_publishes() {
    assert!(publishes("26.3.6", &["26.2.x", "26.3.x"]));
}

#[test]
fn a_development_version_never_publishes() {
    assert!(!publishes("26.4.0", &["26.3.x", "26.4.x"]));
}

#[test]
fn a_prerelease_never_publishes() {
    assert!(!publishes("26.3.6-rc1", &["26.3.x"]));
}

#[test]
fn an_older_line_does_not_publish() {
    assert!(!publishes("26.2.9", &["26.2.x", "26.3.x"]));
}

/// `26.10` is newer than `26.9`: a lexical sort would pick `26.9.x`.
#[test]
fn branches_compare_numerically_not_lexically() {
    assert!(publishes("26.10.1", &["26.9.x", "26.10.x"]));
    assert!(!publishes("26.9.9", &["26.9.x", "26.10.x"]));
}

#[test]
fn a_missing_branch_does_not_publish() {
    assert!(!publishes("26.3.6", &["26.2.x"]));
}

#[test]
fn a_garbage_tag_does_not_publish() {
    assert!(!publishes("v26.3.6", &["26.3.x"]));
    assert!(!publishes("nightly", &["26.3.x"]));
    assert!(!publishes("26.3", &["26.3.x"]));
}

#[test]
fn non_release_branches_are_ignored() {
    assert!(publishes(
        "26.3.6",
        &["main", "backport-26.3.6", "99.x", "26.3.x"]
    ));
}

#[test]
fn a_missing_tag_argument_is_an_error() {
    let out = Command::new("bash")
        .arg(script())
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert!(!out.status.success());
}
