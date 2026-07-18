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
        Some("package") => run_package(),
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
         gen-docs    Regenerate docs/src/cli.md from the command registry\n    \
         package     Build a release tarball for a target\n\n\
         PACKAGE:\n    \
         cargo xtask package --version <VER> --target <TRIPLE> [--bin-dir <DIR>] [--out-dir <DIR>]\n    \
         Assembles <bin-dir>/{{mtui,mtui-mcp}} + dist/ + LICENSE/README into\n    \
         mtui-rs-<VER>-<TRIPLE>.tar.gz (+ .sha256) under <out-dir> (default: dist/release).\n    \
         --bin-dir defaults to target/<TRIPLE>/release."
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

/// Build a release tarball for one target: `cargo xtask package --version <VER>
/// --target <TRIPLE> [--bin-dir <DIR>] [--out-dir <DIR>]`.
///
/// Resolves the workspace-relative default locations (`target/<triple>/release`
/// for binaries, `dist/` for data files, `dist/release/` for output) and hands
/// them to [`xtask::package_target`], whose staging step is offline-tested.
fn run_package() -> Result<()> {
    let root = repo_root()?;
    let args = xtask::PackageArgs::parse(std::env::args().skip(2))?;

    let bin_dir = args
        .bin_dir
        .clone()
        .unwrap_or_else(|| root.join("target").join(&args.target).join("release"));
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| root.join("dist").join("release"));
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating out dir {}", out_dir.display()))?;

    let inputs = xtask::PackageInputs {
        version: &args.version,
        target: &args.target,
        bin_dir: &bin_dir,
        dist_dir: &root.join("dist"),
        root_dir: &root,
        out_dir: &out_dir,
    };
    let tarball = xtask::package_target(&inputs)?;
    println!("wrote {}", tarball.display());
    println!("wrote {}.sha256", tarball.display());
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
