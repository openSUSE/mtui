//! The `mtui-mcp` boot sequence: parse args → resolve config → serve.
//!
//! The Rust analogue of upstream `mtui/mcp/main.py`. It parses [`McpArgs`],
//! initialises a stderr `tracing` subscriber (under stdio, stdout is the
//! JSON-RPC transport), resolves the [`Config`] the way the REPL does, and
//! serves the runtime-synthesised tool surface on the chosen transport:
//!
//! * **stdio** (default) — one process == one client: a single [`McpSession`]
//!   built via [`StdioProvider`] serves the [`McpServer`] over `(stdin, stdout)`
//!   until the client disconnects.
//! * **http** — one process serves many clients: a [`SessionRegistry`] mints a
//!   fresh isolated [`McpServer`] per MCP session (rmcp's streamable-HTTP
//!   transport invokes the factory once per session and owns `Mcp-Session-Id`
//!   keying), mounted on an `axum` router bound to `--host`/`--port`.

use std::sync::Arc;

use clap::Parser;
use mtui_core::{ColorMode, register_all};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tracing_subscriber::EnvFilter;

use crate::args::{McpArgs, Transport};
use crate::provider::{SessionProvider, SessionRegistry, StdioProvider};
use crate::server::McpServer;

/// Run the `mtui-mcp` server: the binary's entire body.
///
/// # Errors
///
/// Returns an error if serving fails for a reason other than a clean client
/// disconnect / Ctrl-C (treated as a clean exit), or — under `--transport http` —
/// if the listener cannot bind `--host`/`--port`.
pub async fn run() -> anyhow::Result<()> {
    let args = McpArgs::parse();

    let color = ColorMode::from(args.color);
    init_tracing(args.debug, color);
    tracing::debug!(debug = args.debug, "mtui-mcp starting");

    match args.transport {
        Transport::Stdio => serve_stdio(&args).await,
        Transport::Http => serve_http(&args).await,
    }
}

/// Serve the tool surface over stdio (one process == one client).
///
/// stdout is the JSON-RPC transport — logging goes to stderr only.
async fn serve_stdio(args: &McpArgs) -> anyhow::Result<()> {
    let server = build_stdio_server(args).await;

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

/// Serve the tool surface over streamable HTTP (one process, many clients).
///
/// rmcp's [`StreamableHttpService`] keys clients by `Mcp-Session-Id` and calls
/// the [`SessionRegistry`] factory once per new session, so each client gets a
/// **fully isolated** [`McpServer`] (own `targets` / `metadata`). The service is
/// a `tower::Service`, mounted as an `axum` fallback and bound to
/// `--host`/`--port`. rmcp defaults `allowed_hosts` to loopback (DNS-rebinding
/// guard); a non-loopback `--host` is out of scope for this bead.
///
/// # Errors
///
/// Returns an error if the TCP listener cannot bind `--host:--port`, or if the
/// server loop fails for a reason other than Ctrl-C.
async fn serve_http(args: &McpArgs) -> anyhow::Result<()> {
    let config = args.resolve_config();
    tracing::info!(
        cap = config.mcp_session_cap,
        idle_timeout_s = config.mcp_session_idle_timeout,
        "mtui-mcp: http transport — per-client session isolation \
         (cap/idle-TTL not yet enforced: mtui-rs-odq8)"
    );

    let registry = Arc::new(register_all());
    let sessions = SessionRegistry::new(registry, config);

    // The factory rmcp invokes once per new MCP session: each call yields a
    // fresh isolated server. Infallible for us, so we always `Ok`.
    let service = StreamableHttpService::new(
        move || Ok(sessions.make_server()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().fallback_service(service);
    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "mtui-mcp: serving on http");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
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
