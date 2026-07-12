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
//! - **http** — one process serves many clients, so a `SessionRegistry` mints a
//!   fresh isolated session per client key (lazy, with an idle-TTL sweeper and a
//!   `max_sessions` cap). That is **P7.10**, the second implementer; it is out
//!   of scope here.
//!
//! Both hand back an `Arc<McpSession>` from the same `get_or_create(key)`
//! signature, which is why the trait — not a concrete session — is the seam.

use std::sync::Arc;

use mtui_config::Config;

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
