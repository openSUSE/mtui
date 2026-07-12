//! Build-time capture of build-provenance metadata for `mtui-mcp --version`.
//!
//! Mirrors `mtui-core`'s `build.rs` so the `mtui-mcp` binary's `-V`/`--version`
//! carries the same `<ver> (<sha>[-dirty], <profile>, <target>)` provenance block
//! as the `mtui` REPL. Build-script env vars do not cross crate boundaries, so
//! each binary crate that wants `MTUI_LONG_VERSION` captures it itself; the logic
//! is intentionally identical. The script never fails the build.

use std::process::Command;

fn main() {
    // Re-run when the checked-out commit or branch changes (a standard
    // build-script limitation: uncommitted edits after a cached build won't flip
    // `-dirty` until something else forces a rerun).
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());

    // clap renders `<bin-name> <long_version>`, so this value must NOT repeat the
    // "mtui-mcp " prefix — it is just the version plus the provenance block.
    let long_version = match git_sha() {
        Some(sha) => format!("{version} ({sha}, {profile}, {target})"),
        None => format!("{version} ({profile}, {target})"),
    };

    println!("cargo:rustc-env=MTUI_LONG_VERSION={long_version}");
}

/// Returns the short commit SHA, suffixed `-dirty` when the working tree has
/// uncommitted changes, or `None` when git is unavailable or this is not a
/// checkout (e.g. a release tarball).
fn git_sha() -> Option<String> {
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
