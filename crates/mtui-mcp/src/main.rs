//! `mtui-mcp` — MCP server that synthesises tools from the command registry.
//!
//! Parses [`McpArgs`](mtui_mcp::args::McpArgs), builds the same [`Config`] the
//! REPL does, and serves the synthesised tool surface over the chosen transport
//! — **stdio** (default, one process == one client) or **http** (many isolated
//! clients). The server modules live in the crate's library target behind the
//! `mcp` feature; a build without that feature links a tiny stub so the `[[bin]]`
//! always exists (feature-matrix gate).
//!
//! Under stdio, **stdout is the JSON-RPC transport** — all logging goes to
//! **stderr**. The `http` transport serves many clients with per-client session
//! isolation (see [`mtui_mcp::run`]).

#[cfg(feature = "mcp")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mtui_mcp::run().await
}

#[cfg(not(feature = "mcp"))]
fn main() -> anyhow::Result<()> {
    // The MCP SDK is compiled in only behind the `mcp` feature. Mirror upstream's
    // "mcp is not installed" hint: fail with a clear, actionable message rather
    // than a silent no-op.
    eprintln!(
        "mtui-mcp was built without the `mcp` feature; rebuild with \
         `cargo build -p mtui-mcp --features mcp`."
    );
    std::process::exit(2);
}
