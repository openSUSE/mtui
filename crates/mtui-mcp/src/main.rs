//! `mtui-mcp` — MCP server that synthesises tools from the command registry.
//!
//! Parses [`McpArgs`](mtui_mcp::args::McpArgs), builds the same [`Config`] the
//! REPL does, and serves the synthesised tool surface over the chosen transport:
//! **stdio** (default, one process == one client) or **http** (many clients with
//! per-client session isolation — see [`mtui_mcp::run`]). Under stdio **stdout is
//! the JSON-RPC transport**, so all logging goes to **stderr**.
//!
//! The server modules live in the library target behind the `mcp` feature; a
//! build without it links a tiny stub so the `[[bin]]` always exists.

#[cfg(feature = "mcp")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mtui_mcp::run().await
}

#[cfg(not(feature = "mcp"))]
fn main() -> anyhow::Result<()> {
    // Fail with an actionable message rather than a silent no-op.
    eprintln!(
        "mtui-mcp was built without the `mcp` feature; rebuild with \
         `cargo build -p mtui-mcp --features mcp`."
    );
    std::process::exit(mtui_core::ExitStatus::Usage.into());
}
