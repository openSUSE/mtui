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
use std::time::Duration;

use clap::Parser;
use mtui_core::{ColorMode, register_all};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, SessionConfig};
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
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
    let keep_alive = session_keep_alive(config.mcp_session_idle_timeout);
    tracing::info!(
        cap = config.mcp_session_cap,
        idle_timeout_s = config.mcp_session_idle_timeout,
        keep_alive =
            keep_alive.map_or_else(|| "disabled".to_owned(), |d| format!("{}s", d.as_secs())),
        "mtui-mcp: http transport — per-client session isolation \
         (session cap + idle-TTL enforced; rmcp keep-alive pinned)"
    );

    let registry = Arc::new(register_all());
    let sessions = SessionRegistry::new(registry, config);

    // Start the idle-TTL sweeper (no-op when session_idle_timeout == 0); a
    // cancellation token lets graceful shutdown stop it cleanly.
    let sweeper_cancel = CancellationToken::new();
    let sweeper = sessions.spawn_sweeper(sweeper_cancel.clone());

    // The factory rmcp invokes once per new MCP session: each call yields a
    // fresh isolated server, or an `Err` (surfaced by rmcp as an internal-error
    // response) once the session cap is reached — a bounded DoS refusal.
    //
    // Pin rmcp's session keep-alive (default 300s) to our idle-TTL: its default
    // is far shorter than our sweeper's horizon and would tear a quiet http
    // session down mid-conversation. `StreamableHttpServerConfig::default()`'s
    // 15s SSE ping cadence is kept — it only keeps the stream warm.
    let factory_sessions = sessions.clone();
    // `LocalSessionManager` / `SessionConfig` are `#[non_exhaustive]`, so build
    // from their defaults and set only the field we override.
    let mut session_config = SessionConfig::default();
    session_config.keep_alive = keep_alive;
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config = session_config;
    let service = StreamableHttpService::new(
        move || factory_sessions.try_make_server(),
        Arc::new(session_manager),
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

    // Stop the sweeper and wait for it to unwind before returning.
    sweeper_cancel.cancel();
    if let Some(handle) = sweeper {
        let _ = handle.await;
    }
    tracing::info!("mtui-mcp: shutting down");
    Ok(())
}

/// The rmcp session keep-alive to pin from `idle_timeout_s`.
///
/// Maps the config's `mcp_session_idle_timeout` to rmcp's
/// [`SessionConfig::keep_alive`]: `0` disables it (matching how the same value
/// disables our own idle sweeper), any positive value becomes that many seconds.
/// This overrides rmcp's 300s default, which is shorter than our sweeper horizon
/// and would otherwise drop a quiet http session.
fn session_keep_alive(idle_timeout_s: u64) -> Option<Duration> {
    (idle_timeout_s != 0).then(|| Duration::from_secs(idle_timeout_s))
}

/// Install a minimal stderr `tracing` subscriber.
///
/// Unlike the REPL's `init_tracing`, this has no runtime-reload handle
/// (`mtui-mcp` never installs a `set_log_level` sink) and no spinner-aware
/// writer (there is no interactive TTY spinner). It writes to **stderr** because
/// stdout carries the MCP JSON-RPC stream. `-d/--debug` and `RUST_LOG` select the
/// level; ANSI follows the resolved [`ColorMode`].
fn init_tracing(debug: bool, color: ColorMode) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_directives(debug)));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(color.resolve())
        .try_init();
}

/// The default `EnvFilter` directive string when `RUST_LOG` is unset.
///
/// Beyond the base level (`debug` under `-d/--debug`, else `info`), this pins
/// `rmcp::service=warn` so the http transport is not flooded by the client's
/// post-completion `notifications/cancelled`: opencode (and other
/// `AbortController`-based streamable-http clients) abort each per-request
/// controller ~10-30ms *after* a successful `tools/call` result, which rmcp logs
/// as a no-op `CancelledNotification` at INFO under `rmcp::service`. Silencing
/// that target to `warn` drops the noise (and rmcp's one-time init breadcrumbs)
/// while keeping every `mtui_*` INFO line. Any explicit `RUST_LOG` takes over
/// completely — this directive only seeds the fallback.
fn default_directives(debug: bool) -> String {
    let base = if debug { "debug" } else { "info" };
    format!("{base},rmcp::service=warn")
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

    #[test]
    fn default_directives_pin_rmcp_service_warn() {
        // The fallback filter (RUST_LOG unset) carries both the base level and
        // the rmcp::service=warn silencer for the http cancellation noise.
        assert_eq!(default_directives(false), "info,rmcp::service=warn");
        assert_eq!(default_directives(true), "debug,rmcp::service=warn");
    }

    #[test]
    fn keep_alive_maps_idle_timeout() {
        // A positive idle-TTL becomes that many seconds; 0 disables keep-alive.
        assert_eq!(
            session_keep_alive(14_400),
            Some(Duration::from_secs(14_400))
        );
        assert_eq!(session_keep_alive(1), Some(Duration::from_secs(1)));
        assert_eq!(session_keep_alive(0), None);
    }

    #[test]
    fn session_manager_pins_keep_alive_from_config() {
        // The manager `serve_http` builds carries our config-derived keep-alive,
        // overriding rmcp's 300s default (the bug that dropped idle sessions).
        let keep_alive = session_keep_alive(14_400);
        let mut session_config = SessionConfig::default();
        session_config.keep_alive = keep_alive;
        let mut manager = LocalSessionManager::default();
        manager.session_config = session_config;
        assert_eq!(
            manager.session_config.keep_alive,
            Some(Duration::from_secs(14_400)),
        );
        assert_ne!(
            manager.session_config.keep_alive,
            Some(SessionConfig::DEFAULT_KEEP_ALIVE),
            "must not inherit rmcp's 300s default",
        );
    }
}
