//! Session resolution seam: [`SessionProvider`] + the stdio [`StdioProvider`].
//!
//! The tool layer (P7.6 `tools`, P7.8 `testreport_tools`) resolves the
//! [`McpSession`] for each call through a [`SessionProvider`], so it never cares
//! which transport it runs under. This mirrors upstream
//! `mtui.mcp.registry.SessionProvider`, which has exactly two implementers:
//!
//! - **stdio** — one process serves one client, so a single session is reused
//!   for every call (the `key` is accepted and ignored). That is
//!   [`StdioProvider`], built here.
//! - **http** — one process serves many clients, so each client gets a fresh
//!   isolated session. Under rmcp's streamable-HTTP transport this isolation is
//!   bound by the [`SessionRegistry`] *factory* (`make_server`), which the
//!   transport calls once per new MCP session; rmcp's session manager owns the
//!   `Mcp-Session-Id` keying and teardown, so — unlike upstream's
//!   application-owned registry — we do not reimplement a dict/sweeper here.
//!
//! Both stdio and http hand back an `Arc<McpSession>` from the same
//! `get_or_create(key)` signature, which is why the trait — not a concrete
//! session — is the seam.

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::Registry;

use crate::server::McpServer;
use crate::session::McpSession;

/// The minimal surface the tool layer resolves a session through.
///
/// One async method maps a per-client `key` to the [`McpSession`] the call
/// should dispatch against, minting one on first use where applicable. Under
/// stdio the key is ignored (single session); under the future http registry it
/// selects the caller's isolated session.
///
/// The trait uses a native `async fn` and is consumed by a concrete provider
/// type (the rmcp `ServerHandler` is not `dyn`-compatible, and stdio has exactly
/// one provider), so no `dyn SessionProvider` boxing is required.
pub trait SessionProvider {
    /// Returns the session bound to `key`, minting one if needed.
    ///
    /// `key` identifies the MCP client. Single-session providers (stdio) ignore
    /// it and always return the same session.
    fn get_or_create(&self, key: &str) -> impl Future<Output = Arc<McpSession>> + Send;
}

/// The stdio single-session provider: one [`McpSession`] reused for every call.
///
/// One `mtui-mcp` process over stdio serves exactly one client, so there is no
/// per-client isolation to do — every `get_or_create` returns the same session
/// regardless of `key`. This is the Rust analogue of upstream `McpSession`
/// doubling as the degenerate single-entry provider (`get_or_create` returning
/// `self`).
#[derive(Clone)]
pub struct StdioProvider {
    session: Arc<McpSession>,
}

impl StdioProvider {
    /// Builds the provider's single headless session from `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            session: McpSession::new(config),
        }
    }

    /// The single session this provider owns (also the direct handle callers can
    /// use without going through [`SessionProvider::get_or_create`]).
    #[must_use]
    pub fn session(&self) -> Arc<McpSession> {
        Arc::clone(&self.session)
    }
}

impl SessionProvider for StdioProvider {
    async fn get_or_create(&self, _key: &str) -> Arc<McpSession> {
        // Single-entry: the key is intentionally ignored — one process, one
        // session. (Per-client keying is the http registry's job, P7.10.)
        Arc::clone(&self.session)
    }
}

/// The http per-client session factory.
///
/// Under `--transport http` one `mtui-mcp` process serves many concurrent MCP
/// clients, and each must see **only its own** loaded template + SSH `targets`;
/// sharing one session would let one client's `load_template` clobber another's.
/// This registry mints a **fresh, fully isolated** [`McpServer`] (with its own
/// [`McpSession`]) per new MCP session via [`make_server`](Self::make_server) —
/// the closure rmcp's `StreamableHttpService` invokes once per session.
///
/// This is the Rust analogue of upstream `mtui.mcp.registry.SessionRegistry`,
/// but far thinner: rmcp's `LocalSessionManager` already keys sessions by
/// `Mcp-Session-Id` and drives their lifecycle, so this type owns **no** session
/// map, lock, or idle sweeper. The `[mcp] session_cap` / `session_idle_timeout`
/// bounds are parsed but not yet enforced here — that is follow-up `mtui-rs-odq8`.
#[derive(Clone)]
pub struct SessionRegistry {
    /// The shared command registry every minted server dispatches against.
    registry: Arc<Registry>,
    /// The base config each session is cloned from (per-session isolation of any
    /// scalar a command rebinds on `config` — mirrors upstream `build_session`'s
    /// shallow copy).
    config: Config,
}

impl SessionRegistry {
    /// Builds the factory from the shared command `registry` and a base `config`.
    #[must_use]
    pub fn new(registry: Arc<Registry>, config: Config) -> Self {
        Self { registry, config }
    }

    /// Mint a fresh, isolated [`McpSession`] from the base config.
    ///
    /// Clones the base [`Config`] so the new session's mutable scalar state is
    /// independent (own `metadata` / `targets` / capture sink). This is the
    /// isolation boundary; [`make_server`](Self::make_server) wraps it for the
    /// transport, and tests use it directly to assert per-session isolation.
    #[must_use]
    pub fn make_session(&self) -> Arc<McpSession> {
        McpSession::new(self.config.clone())
    }

    /// Mint a fresh, isolated [`McpServer`] for one MCP session.
    ///
    /// Builds a fresh [`McpSession`] via [`make_session`](Self::make_session) and
    /// wires it into an [`McpServer`] sharing the (read-only) command registry.
    /// Called once per new session by the streamable-HTTP transport's
    /// `service_factory`.
    #[must_use]
    pub fn make_server(&self) -> McpServer {
        McpServer::new(Arc::clone(&self.registry), self.make_session())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdio provider is single-entry: any two keys resolve to the *same*
    /// session instance, mirroring upstream `McpSession.get_or_create` returning
    /// `self` regardless of key.
    #[tokio::test]
    async fn stdio_provider_returns_same_session_for_any_key() {
        let provider = StdioProvider::new(Config::default());

        let a = provider.get_or_create("client-a").await;
        let b = provider.get_or_create("client-b").await;
        let direct = provider.session();

        assert!(
            Arc::ptr_eq(&a, &b),
            "stdio provider must return the same session for different keys"
        );
        assert!(
            Arc::ptr_eq(&a, &direct),
            "get_or_create must return the provider's single session"
        );
    }

    /// The resolved session exposes the guarded [`Session`] and capture sink the
    /// dispatch path (P7.6) needs.
    #[tokio::test]
    async fn resolved_session_exposes_dispatch_seams() {
        let provider = StdioProvider::new(Config::default());
        let session = provider.get_or_create("<default>").await;

        // Both seams are reachable and the sink starts empty.
        let _guard = session.session().lock().await;
        assert_eq!(session.output().take(), "");
    }
}
