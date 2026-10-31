//! Build-time capture of build-provenance metadata for `--version`.
//!
//! A statically-compiled Rust binary has no runtime dependency-version drift —
//! every dep is compiled in at a lockfile-pinned version — so `--version` has
//! no need for a block reporting those. What *does* vary for an out-of-tree
//! build (someone building `mtui` outside a standard system package) is the
//! build provenance: which commit, which profile, which target. This build
//! script captures that into the `MTUI_LONG_VERSION` env var, which `args.rs`
//! feeds to clap's `long_version`.
//!
//! clap renders `mtui <long_version>`, so the captured value omits the leading
//! `mtui ` and the final line reads `mtui <ver> (<ref>[-dirty], <profile>,
//! <target>)`, where `<ref>` is `git describe` output restricted to release tags
//! of the crate's own `MAJOR.MINOR` series (`26.3.3-2-gabcdef012345`), or a bare
//! short SHA when no such tag is reachable. When built from a packaged crate
//! (`.cargo_vcs_info.json` beside the manifest) or inside a foreign checkout
//! (a dotfiles `$HOME`, a vendored tree), the ref field is omitted: stamping
//! a foreign SHA into `--version` is worse than stamping nothing. The script
//! never fails the build.

use std::path::Path;
use std::process::Command;

fn main() {
    // A packaged crate carries `.cargo_vcs_info.json` beside its manifest,
    // and `../../.git/HEAD` points outside the package there — emitting it
    // would rerun this script on every build. Skip both there.
    let packaged = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".cargo_vcs_info.json")
        .is_file();
    if !packaged {
        // Re-run when the checked-out commit or branch changes. This catches
        // commits and branch switches; uncommitted edits made *after* a cached build
        // won't flip the `-dirty` flag until something else forces a rerun — a
        // standard build-script limitation we accept rather than watch the whole tree.
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    }

    let version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());

    // clap renders `<bin-name> <long_version>`, so this value must NOT repeat
    // the "mtui " prefix — it is just the version plus the provenance block.
    let long_version = match (!packaged).then(git_ref).flatten() {
        Some(git_ref) => format!("{version} ({git_ref}, {profile}, {target})"),
        None => format!("{version} ({profile}, {target})"),
    };

    println!("cargo:rustc-env=MTUI_LONG_VERSION={long_version}");
}

/// True when git resolves this workspace as its own toplevel, i.e. the build
/// runs from the workspace's checkout rather than from a tarball sitting
/// inside some foreign repository.
fn git_toplevel_is_workspace_root() -> bool {
    let toplevel = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let Some(toplevel) = toplevel else {
        return false;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (Ok(toplevel), Ok(root)) = (Path::new(&toplevel).canonicalize(), root.canonicalize())
    else {
        return false;
    };
    toplevel == root
}

/// Returns a human-readable git ref for the build: `git describe` output when a
/// release tag of the crate's own `MAJOR.MINOR.*` series is reachable (e.g.
/// `26.3.3-2-gabcdef012345-dirty`), otherwise a bare short SHA (e.g.
/// `abcdef012345-dirty`). Returns `None` when git is unavailable, when this is
/// a packaged crate's build, or when git's toplevel is not the workspace root
/// (a foreign checkout would stamp its own SHA).
///
/// Matching only the own series keeps unrelated tags (`archive/python-main`,
/// legacy `16.x`) out of the ref; `--always` degrades to the short SHA, and
/// `git_short_sha` covers the case where `describe` fails but `rev-parse` works.
fn git_ref() -> Option<String> {
    if !git_toplevel_is_workspace_root() {
        return None;
    }
    let pattern = format!(
        "{}.{}.*",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR")
    );
    let out = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--match",
            &pattern,
            "--always",
            "--dirty",
            "--long",
            "--abbrev=12",
        ])
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
