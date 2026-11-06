//! Build-time capture of build-provenance metadata for `mtui-mcp --version`.
//!
//! Mirrors `mtui-core`'s `build.rs` so `mtui-mcp -V` carries the same
//! `<ver> (<sha>[-dirty], <profile>, <target>)` block as the REPL. Build-script
//! env vars do not cross crate boundaries, so each binary crate captures
//! `MTUI_LONG_VERSION` itself. When built from a packaged crate or inside a
//! foreign checkout the ref field is omitted, like `mtui-core`. The script
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
        // Re-run when the checked-out commit or branch changes. Uncommitted edits
        // after a cached build will not flip `-dirty` until something forces a rerun.
        println!("cargo:rerun-if-changed=../../.git/HEAD");
    }

    let version = env!("CARGO_PKG_VERSION");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());

    // clap renders `<bin-name> <long_version>`, so this must not repeat the
    // "mtui-mcp " prefix.
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
/// `--always` degrades to the short SHA; `git_short_sha` covers the case where
/// `describe` fails but `rev-parse` still works.
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
