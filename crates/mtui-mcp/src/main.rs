//! `mtui-mcp` — MCP server that synthesises tools from the command registry.
//!
//! Phase 0 stub: parses `--help`/`--version` via clap and exits 0. The rmcp
//! stdio server lands in Phase 7.

use clap::Parser;

/// MCP server for mtui-rs (tools synthesised from the command registry).
#[derive(Debug, Parser)]
#[command(name = "mtui-mcp", version, about, long_about = None)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("mtui-mcp {} (Phase 0 stub)", env!("CARGO_PKG_VERSION"));
    Ok(())
}
