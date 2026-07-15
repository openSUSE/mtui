//! `cargo xtask` — repo automation (the standard `xtask` pattern).
//!
//! Today it has one job: **regenerate the checked-in packaging artifacts** under
//! `dist/` — shell completions (bash/zsh/fish) and man pages for both binaries
//! (`mtui`, `mtui-mcp`). Upstream mtui shipped completions via `argcomplete` at
//! runtime; mtui-rs pre-generates them from the two top-level `clap` parsers so
//! the rpm spec (`%files`) and any packaging can consume them without a build.
//!
//! Run with `cargo xtask gen` (see the `.cargo/config.toml` alias). The actual
//! generation lives in [`xtask::generate_into`] so it is unit-testable offline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("gen") => run_gen(),
        Some(other) => {
            eprintln!("unknown task: {other}\n");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!(
        "xtask - mtui-rs repo automation\n\n\
         USAGE:\n    cargo xtask <TASK>\n\n\
         TASKS:\n    gen    Regenerate dist/ completions + man pages for both binaries"
    );
}

/// Regenerate `dist/completions/{bash,zsh,fish}/…` and `dist/man/*.1` into the
/// repo's checked-in `dist/` tree.
fn run_gen() -> Result<()> {
    let dist = repo_root()?.join("dist");
    xtask::generate_into(&dist)?;
    println!("wrote completions + man pages under {}", dist.display());
    Ok(())
}

/// The workspace root: the parent of this crate's manifest dir (`xtask/`).
fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .map(Path::to_path_buf)
        .context("xtask manifest dir has no parent")?;
    if !root.join("Cargo.toml").is_file() {
        bail!(
            "could not locate workspace root from {}",
            manifest.display()
        );
    }
    Ok(root)
}
