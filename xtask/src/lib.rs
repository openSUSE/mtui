//! Generation helpers for the `xtask gen` task, factored out of `main.rs` so the
//! completion + man-page emission is unit-testable offline (into a `tempdir`)
//! without touching the checked-in `dist/` tree.
//!
//! Both binary parsers derive `clap::Parser`, hence `CommandFactory`, so
//! [`mtui_core::Args::command`] / [`mtui_mcp::McpArgs::command`] hand back the
//! `clap::Command` fed to `clap_complete` and `clap_mangen`. `mtui-mcp` is linked
//! **without** its `mcp` feature: only the (ungated) `args` module is used,
//! keeping the rmcp/axum server graph out of this dev tool's build.

use std::path::Path;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;

/// The three shells the Phase 8 DoD names explicitly.
pub const SHELLS: [Shell; 3] = [Shell::Bash, Shell::Zsh, Shell::Fish];

/// Generate bash/zsh/fish completions + man pages for **both** binaries into the
/// `dist/` layout rooted at `dist`: `dist/completions/{shell}/…` and
/// `dist/man/<bin>.1`. Creates the directories if missing.
pub fn generate_into(dist: &Path) -> Result<()> {
    let completions = dist.join("completions");
    let man = dist.join("man");
    for shell in SHELLS {
        std::fs::create_dir_all(completions.join(shell.to_string()))
            .with_context(|| format!("creating completions dir for {shell}"))?;
    }
    std::fs::create_dir_all(&man).context("creating man dir")?;

    // `command()` uses the `#[command(name = ...)]` set on each parser, so the
    // generated file names / man titles are already `mtui` and `mtui-mcp`.
    gen_binary(&mut mtui_core::Args::command(), "mtui", &completions, &man)?;
    gen_binary(
        &mut mtui_mcp::McpArgs::command(),
        "mtui-mcp",
        &completions,
        &man,
    )?;
    Ok(())
}

/// Emit the bash/zsh/fish completions and the man page for one binary.
fn gen_binary(cmd: &mut clap::Command, bin: &str, completions: &Path, man: &Path) -> Result<()> {
    for shell in SHELLS {
        clap_complete::generate_to(shell, cmd, bin, completions.join(shell.to_string()))
            .with_context(|| format!("generating {shell} completion for {bin}"))?;
    }
    // Both parsers set `version` AND `long_version` to `MTUI_LONG_VERSION`, a
    // build-provenance string (`<ver> (<sha>-dirty, <profile>, <target>)`). That
    // is right for a live `--version` but poisons a *checked-in* man page (title
    // + VERSION section) with the commit sha / dirty flag / host target, making it
    // churn on every build. mangen renders `long_version` in the VERSION section,
    // so pin *both* to the plain crate version for a stable, committable artifact.
    let stable = cmd
        .clone()
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(env!("CARGO_PKG_VERSION"));
    let page = clap_mangen::Man::new(stable);
    let mut buf = Vec::new();
    page.render(&mut buf)
        .with_context(|| format!("rendering man page for {bin}"))?;
    let man_path = man.join(format!("{bin}.1"));
    std::fs::write(&man_path, &buf).with_context(|| format!("writing {}", man_path.display()))?;
    Ok(())
}
