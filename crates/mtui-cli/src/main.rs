//! `mtui` — interactive REPL + non-interactive single-command entry point.
//!
//! Phase 0 stub: parses `--help`/`--version` via clap and exits 0. The REPL and
//! command dispatch land in Phase 6.

use clap::Parser;

/// Maintenance Test Update Installer (Rust rewrite of openSUSE/mtui).
#[derive(Debug, Parser)]
#[command(name = "mtui", version, about, long_about = None)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("mtui {} (Phase 0 stub)", env!("CARGO_PKG_VERSION"));
    Ok(())
}
