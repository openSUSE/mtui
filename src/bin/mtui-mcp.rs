//! `mtui-mcp` — the MCP server binary. See [`mtui_mcp::run`] for the entry
//! point implementation.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mtui_mcp::run().await
}
