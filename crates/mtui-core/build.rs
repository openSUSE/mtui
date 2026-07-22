//! Build-time capture of build-provenance metadata for `--version`.
//!
//! Upstream mtui's `--version` listed separately-installed *runtime* dependency
//! versions (paramiko, openqa-client, …) because those could drift per operator
//! environment. A statically-compiled Rust binary has no such drift — every dep
//! is compiled in at a lockfile-pinned version — so that block would be
//! redundant. What *does* vary for an out-of-tree build (someone building
//! `mtui` outside a standard system package) is the build provenance: which
//! commit, which profile, which target. This build script captures that into the
//! `MTUI_LONG_VERSION` env var, which `args.rs` feeds to clap's `long_version`.
//!
//! clap renders `mtui <long_version>`, so the captured value omits the leading
//! `mtui ` and the final line reads `mtui <ver> (<ref>[-dirty], <profile>,
//! <target>)`, where `<ref>` is `git describe --tags --always --dirty --long`
//! output — a tag-relative name once releases are tagged (`v1.2.0-3-gabcdef`),
//! or a bare short SHA until then. When built outside a git checkout the ref
//! field is omitted; the profile and target are always present. The script never
//! fails the build.

use std::process::Command;

fn main() {
    // Re-run when the checked-out commit or branch changes. This catches
    // commits and branch switches; uncommitted edits made *after* a cached build
    // won't flip the `-dirty` flag until something else forces a rerun — a
    // standard build-script limitation we accept rather than watch the whole tree.
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());

    // clap renders `<bin-name> <long_version>`, so this value must NOT repeat
    // the "mtui " prefix — it is just the version plus the provenance block.
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
/// `git describe --tags --always --dirty --long` already degrades to the short
/// SHA when no tag is reachable, so today (no tags) it prints the same short-SHA
/// form as the previous `rev-parse` scheme; once releases are tagged it upgrades
/// to the tag-relative form automatically. `git_short_sha` is kept as an explicit
/// fallback for the case where `describe` fails but `rev-parse` still works.
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
