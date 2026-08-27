//! Build-time capture of build-provenance metadata for `mtui-mcp --version`.
//!
//! Mirrors `mtui-core`'s `build.rs` so `mtui-mcp -V` carries the same
//! `<ver> (<sha>[-dirty], <profile>, <target>)` block as the REPL. Build-script
//! env vars do not cross crate boundaries, so each binary crate captures
//! `MTUI_LONG_VERSION` itself. The script never fails the build.

use std::process::Command;

fn main() {
    // Re-run when the checked-out commit or branch changes. Uncommitted edits
    // after a cached build will not flip `-dirty` until something forces a rerun.
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());

    // clap renders `<bin-name> <long_version>`, so this must not repeat the
    // "mtui-mcp " prefix.
    let long_version = match git_ref() {
        Some(git_ref) => format!("{version} ({git_ref}, {profile}, {target})"),
        None => format!("{version} ({profile}, {target})"),
    };

    println!("cargo:rustc-env=MTUI_LONG_VERSION={long_version}");
}

/// Returns a human-readable git ref for the build: `git describe` output when a
/// tag is reachable (e.g. `v1.2.0-3-gabcdef-dirty`), otherwise a bare short SHA
/// (e.g. `abcdef012345-dirty`). Returns `None` when git is unavailable or this is
/// not a checkout (e.g. a release tarball).
///
/// `describe` already degrades to the short SHA when no tag is reachable and
/// upgrades to the tag-relative form once releases are tagged; `git_short_sha`
/// covers the case where `describe` fails but `rev-parse` still works.
fn git_ref() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty", "--long"])
        .output()
        .ok()?;
    if out.status.success()
        && let Ok(desc) = String::from_utf8(out.stdout)
    {
        let desc = desc.trim();
        if !desc.is_empty() {
            return Some(desc.to_owned());
        }
    }
    git_short_sha()
}

/// Returns the short commit SHA, suffixed `-dirty` when the working tree has
/// uncommitted changes, or `None` when git is unavailable.
fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        return None;
    }

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());

    Some(if dirty {
        format!("{sha}-dirty")
    } else {
        sha.to_owned()
    })
}
