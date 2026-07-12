//! The `mtui-mcp` boot sequence: parse args → resolve config → serve on stdio.
//!
//! The Rust analogue of upstream `mtui/mcp/main.py`, scoped to the **stdio**
//! transport (one process == one client). It parses [`McpArgs`], initialises a
//! stderr-only `tracing` subscriber (stdout is the JSON-RPC transport), resolves
//! the [`Config`] the way the REPL does, builds a single [`McpSession`] via the
//! [`StdioProvider`], and serves the runtime-synthesised [`McpServer`] over
//! `(stdin, stdout)` until the client disconnects.
//!
//! The `http` transport (per-client session registry) is bead `mtui-rs-76e.10`;
//! here it is rejected with a clear not-yet-implemented error.

use std::sync::Arc;

use clap::Parser;
use mtui_core::{ColorMode, register_all};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use crate::args::{McpArgs, Transport};
use crate::provider::{SessionProvider, StdioProvider};
use crate::server::McpServer;

/// Run the `mtui-mcp` server: the binary's entire body.
///
/// # Errors
///
/// Returns an error if the `http` transport is requested (not yet implemented),
/// or if serving over stdio fails for a reason other than a clean client
/// disconnect / Ctrl-C (which are treated as a clean exit).
pub async fn run() -> anyhow::Result<()> {
    let args = McpArgs::parse();

    let color = ColorMode::from(args.color);
    init_tracing(args.debug, color);
    tracing::debug!(debug = args.debug, "mtui-mcp starting");

    ensure_transport_supported(args.transport)?;

    let server = build_stdio_server(&args).await;

    tracing::info!("mtui-mcp: serving on stdio");

    // `serve` runs the initialize handshake then the request loop over
    // (stdin, stdout). stdout is the transport — logging goes to stderr only.
    let running = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;

    // Block until the peer disconnects (or Ctrl-C ends the loop); a clean
    // disconnect is a normal exit.
    running.waiting().await?;
    tracing::info!("mtui-mcp: shutting down");
    Ok(())
}

/// Install a minimal stderr `tracing` subscriber.
///
/// Unlike the REPL's `init_tracing`, this has no runtime-reload handle
/// (`mtui-mcp` never installs a `set_log_level` sink) and no spinner-aware
/// writer (there is no interactive TTY spinner). It writes to **stderr** because
/// stdout carries the MCP JSON-RPC stream. `-d/--debug` and `RUST_LOG` select the
/// level; ANSI follows the resolved [`ColorMode`].
fn init_tracing(debug: bool, color: ColorMode) {
    let default = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(color.resolve())
        .try_init();
}

/// Reject transports this build does not serve.
///
/// Only `stdio` is implemented; `http` (per-client session isolation) is bead
/// `mtui-rs-76e.10`. Refusing here — rather than silently downgrading `http` to a
/// shared session — keeps the isolation contract honest.
///
/// # Errors
///
/// Returns an error naming the follow-up bead when `transport` is [`Transport::Http`].
fn ensure_transport_supported(transport: Transport) -> anyhow::Result<()> {
    if transport == Transport::Http {
        anyhow::bail!(
            "--transport http is not yet implemented (mtui-rs-76e.10); use the default \
             stdio transport"
        );
    }
    Ok(())
}

/// Build the runtime-synthesised stdio server from resolved args.
///
/// Resolves the [`Config`](mtui_config::Config) the same way the REPL does (file
/// chain + CLI overrides), then mints the single headless [`McpSession`] via the
/// [`StdioProvider`] (stdio = one process = one session) and wires it into an
/// [`McpServer`]. Factored out of [`run`] so the wiring is testable without the
/// blocking stdio serve loop.
async fn build_stdio_server(args: &McpArgs) -> McpServer {
    let config = args.resolve_config();
    let registry = Arc::new(register_all());
    let provider = StdioProvider::new(config);
    let session = provider.get_or_create("<default>").await;
    McpServer::new(registry, session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rmcp::handler::server::ServerHandler;

    fn args(argv: &[&str]) -> McpArgs {
        let mut full = vec!["mtui-mcp"];
        full.extend_from_slice(argv);
        McpArgs::try_parse_from(full).expect("args parse")
    }

    #[test]
    fn stdio_transport_is_supported() {
        assert!(ensure_transport_supported(Transport::Stdio).is_ok());
    }

    #[test]
    fn http_transport_is_rejected_with_bead_reference() {
        let err = ensure_transport_supported(Transport::Http).expect_err("http must be rejected");
        assert!(
            err.to_string().contains("mtui-rs-76e.10"),
            "error should name the follow-up bead: {err}"
        );
    }

    #[tokio::test]
    async fn build_stdio_server_wires_the_synthesised_surface() {
        // A built server reports tools capability — proving the handler is wired.
        let server = build_stdio_server(&args(&[])).await;
        assert!(
            server.get_info().capabilities.tools.is_some(),
            "server should advertise the tools capability"
        );
    }
}
