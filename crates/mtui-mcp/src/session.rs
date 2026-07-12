//! Per-client MCP session (placeholder — P7.2 scope).
//!
//! [`McpSession`] is the headless mtui session that backs one `mtui-mcp` client.
//! It is the Rust analogue of upstream `mtui.mcp.session.McpSession`: it owns the
//! mutable [`Session`] state a command dispatches against plus the [`SharedBuf`]
//! sink that captures the command's display output for the tool result.
//!
//! Under **stdio** one instance serves the single client; under **http** the
//! future `SessionRegistry` (P7.10) owns one instance per client. In both cases
//! the [`crate::provider::SessionProvider`] seam hands callers an
//! `Arc<McpSession>`, so the tool layer (P7.6/P7.8) stays transport-agnostic.
//!
//! ## Scope (P7.2 vs P7.3)
//!
//! This is the **placeholder** introduced by P7.2 so the provider trait can
//! return the real `Arc<McpSession>` type without a later signature change. It
//! carries only what the P7.1 spike proved is needed: the guarded [`Session`]
//! and the capture sink, a constructor, and accessors.
//!
//! TODO(P7.3): grow this type in place (do not replace it) with the internals
//! upstream `McpSession` eventually holds:
//!   - a per-RRID serialiser layered over a shared/exclusive registry gate
//!     (upstream `_RWLock` + `_rrid_locks`), so same-RRID calls serialise while
//!     different-RRID calls run concurrently and registry mutations get an
//!     exclusive view;
//!   - the background-job table (`_jobs`) for the async/`start_job` path;
//!   - `close()` host teardown (disconnect every loaded template's hosts,
//!     release pool claims) — owned by the http registry's idle sweep (P7.10);
//!   - the non-interactive contract wiring (`interactive = false`, unset
//!     prompter) so TTY-less MCP calls take command defaults;
//!   - output-cap enforcement bolted onto [`SharedBuf::take`].

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::Session;
use tokio::sync::Mutex;

use crate::capture::{self, SharedBuf};

/// A headless mtui session backing one MCP client.
///
/// Holds the [`Session`] behind a [`Mutex`] because command dispatch
/// ([`mtui_core::dispatch_argv`]) needs `&mut Session` while the rmcp
/// `ServerHandler` methods take `&self` (P7.1 spike finding). The paired
/// [`SharedBuf`] is the sink the session's display writes to; a tool call
/// [`take`](SharedBuf::take)s it to isolate its own output.
pub struct McpSession {
    /// The guarded session commands dispatch against.
    session: Arc<Mutex<Session>>,
    /// The capture sink the session's display writes to; drained per tool call.
    output: SharedBuf,
}

impl McpSession {
    /// Builds a headless session from `config`, wiring its display to a fresh
    /// capture sink, and returns it as an `Arc` (the shape the provider hands
    /// out).
    ///
    /// The session is non-interactive with color disabled — see
    /// [`capture::session`].
    #[must_use]
    pub fn new(config: Config) -> Arc<Self> {
        let (session, output) = capture::session(config);
        Arc::new(Self {
            session: Arc::new(Mutex::new(session)),
            output,
        })
    }

    /// The guarded session, for dispatch under the session lock.
    #[must_use]
    pub fn session(&self) -> &Arc<Mutex<Session>> {
        &self.session
    }

    /// The capture sink, drained per tool call to isolate that call's output.
    #[must_use]
    pub fn output(&self) -> &SharedBuf {
        &self.output
    }
}
