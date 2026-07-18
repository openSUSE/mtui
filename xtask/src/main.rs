//! `cargo xtask` — repo automation (the standard `xtask` pattern).
//!
//! Today it has one job: **regenerate the checked-in packaging artifacts** under
//! `dist/` — shell completions (bash/zsh/fish) and man pages for both binaries
//! (`mtui`, `mtui-mcp`). Upstream mtui shipped completions via `argcomplete` at
//! runtime; mtui-rs pre-generates them from the two top-level `clap` parsers so
//! the rpm spec (`%files`) and any packaging can consume them without a build.
//!
//! Run with `cargo xtask gen` (see the `.cargo/config.toml` alias). A second
//! task, `cargo xtask gen-docs`, regenerates the mdBook CLI reference
//! (`docs/src/cli.md`) from the command registry. The actual generation lives in
//! [`xtask::generate_into`] / [`xtask::generate_docs_into`] so it is
//! unit-testable offline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("gen") => run_gen(),
        Some("gen-docs") => run_gen_docs(),
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
         TASKS:\n    \
         gen         Regenerate dist/ completions + man pages for both binaries\n    \
         gen-docs    Regenerate docs/src/cli.md from the command registry"
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

/// Regenerate `docs/src/cli.md` (the mdBook CLI reference) from the command
/// registry into the repo's checked-in `docs/` tree.
fn run_gen_docs() -> Result<()> {
    let src = repo_root()?.join("docs").join("src");
    xtask::generate_docs_into(&src)?;
    println!(
        "wrote {} under {}",
        xtask::CLI_REFERENCE_FILE,
        src.display()
    );
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
