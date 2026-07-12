//! Per-client MCP session.
//!
//! [`McpSession`] is the headless mtui session that backs one `mtui-mcp` client.
//! It is the Rust analogue of upstream `mtui.mcp.session.McpSession`: it owns the
//! mutable [`Session`] state a command dispatches against plus the [`SharedBuf`]
//! sink that captures the command's display output for the tool result, and
//! exposes [`run_command`](McpSession::run_command) — the central dispatch
//! primitive the tool layer calls (drain → dispatch → capture → output-cap).
//!
//! Under **stdio** one instance serves the single client; under **http** the
//! future `SessionRegistry` (P7.10) owns one instance per client. In both cases
//! the [`crate::provider::SessionProvider`] seam hands callers an
//! `Arc<McpSession>`, so the tool layer (P7.6/P7.8) stays transport-agnostic.
//!
//! ## Scope (landed vs deferred)
//!
//! P7.3 landed the dispatch primitive: `run_command`, the [`McpCommandError`]
//! failure envelope, and the per-result output cap (`[mcp] max_output_bytes`).
//! The non-interactive contract (`interactive = false`, unset prompter) is
//! already provided by [`capture::session`] passing `is_repl = false`.
//!
//! **Deliberately deferred** to their own follow-up beads (this type grows in
//! place — do not replace it):
//!   - per-template concurrency — a per-RRID serialiser over a shared/exclusive
//!     registry gate (upstream `_RWLock` + `_rrid_locks`). Today every call
//!     serialises behind the single session `Mutex`; see bead `mtui-rs-76e.11`;
//!   - the background-job table (`_jobs`) + job tools — `mtui-rs-76e.12`;
//!   - `close()` host teardown (disconnect every loaded template's hosts,
//!     release pool claims), owned by the http idle sweep — `mtui-rs-76e.13`;
//!   - `notifications/progress` heartbeats for long calls — `mtui-rs-76e.14`.

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::{EngineError, Registry, Session, dispatch_argv};
use tokio::sync::Mutex;

use crate::capture::{self, SharedBuf};
use crate::slim::cap_output;

/// A command dispatch that failed under the MCP transport.
///
/// The Rust analogue of upstream `mtui.mcp.session.McpCommandError`: it carries
/// the streams captured during the failed run so the server layer can surface
/// them to the client:
///
/// * `stdout` — everything the command printed before failing (already capped).
/// * `stderr` — the parse/usage complaint or the command-error message.
/// * `exit_code` — argparse-style status: `2` for a usage/parse error, `1` for
///   an unknown command or a command-body failure.
///
/// [`Display`](std::fmt::Display) renders a one-line summary plus the captured
/// stderr so the default MCP error envelope is human-readable (mirrors
/// upstream's `_render`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCommandError {
    /// Captured stdout up to the point of failure (already output-capped).
    pub stdout: String,
    /// Captured stderr (parse/usage text, command-error message).
    pub stderr: String,
    /// Non-zero exit code: `2` for parse/usage errors, `1` otherwise.
    pub exit_code: i32,
}

impl std::fmt::Display for McpCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command failed (exit_code={})", self.exit_code)?;
        let tail = self.stderr.trim();
        if !tail.is_empty() {
            write!(f, ": {tail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for McpCommandError {}

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
    /// Per-result output-size budget (bytes), from `config.mcp_max_output_bytes`.
    /// `0` disables the cap. Retained here so [`run_command`](Self::run_command)
    /// need not hold the whole [`Config`].
    max_output_bytes: usize,
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
        let max_output_bytes = config.mcp_max_output_bytes;
        let (session, output) = capture::session(config);
        Arc::new(Self {
            session: Arc::new(Mutex::new(session)),
            output,
            max_output_bytes,
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

    /// The per-result output-size budget in bytes (`0` disables the cap).
    ///
    /// Exposed for the hand-written testreport tools ([`crate::testreport_tools`]),
    /// which cap their file-content payloads with the same
    /// [`cap_output`](crate::slim::cap_output) budget `run_command` applies.
    #[must_use]
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Runs a registered command and returns its captured, output-capped stdout.
    ///
    /// The central MCP dispatch primitive (the Rust analogue of upstream
    /// `McpSession.run_command`): it drains any stale captured output, dispatches
    /// `name`/`argv` through the **same** engine entry the REPL uses
    /// ([`mtui_core::dispatch_argv`]) under the session lock, then returns what the command
    /// wrote to the captured display — passed through [`cap_output`] so one large
    /// result cannot dwarf the client's context.
    ///
    /// The whole call holds the single session lock, so concurrent tool calls
    /// serialise (whole-session, not per-template — see the module `TODO`). A
    /// `--help`/`--version` request is a *success* (its text is returned),
    /// matching argparse's exit-0 semantics.
    ///
    /// # Errors
    ///
    /// Returns [`McpCommandError`] when argument parsing fails
    /// (`exit_code == 2`), the command is unknown, or the command body fails
    /// (`exit_code == 1`). The error carries the (capped) stdout produced before
    /// the failure plus the failure text as stderr.
    pub async fn run_command(
        &self,
        registry: &Registry,
        name: &str,
        argv: &[String],
    ) -> Result<String, McpCommandError> {
        // Isolate this call's output: drop anything a prior call left behind.
        let _ = self.output.take();

        let result = {
            let mut session = self.session.lock().await;
            dispatch_argv(registry, &mut session, name, argv).await
        };

        let text = cap_output(self.output.take(), self.max_output_bytes);

        match result {
            Ok(()) => Ok(text),
            // `--help`/`--version` is argparse-exit-0: return its text as a
            // success, not an error. clap renders help into the `Parse` message
            // (not the display sink), so surface that (capped); a genuine usage
            // error is exit 2 (below).
            Err(EngineError::Parse {
                help_or_version: true,
                message,
            }) => Ok(cap_output(message, self.max_output_bytes)),
            Err(err) => {
                let (stderr, exit_code) = match &err {
                    EngineError::Parse { message, .. } => (message.clone(), 2),
                    other => (other.to_string(), 1),
                };
                Err(McpCommandError {
                    stdout: text,
                    stderr,
                    exit_code,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_core::register_all;

    fn session(config: Config) -> Arc<McpSession> {
        McpSession::new(config)
    }

    /// A fresh session honours the non-interactive contract: no prompter is
    /// wired (upstream `prompter = None`; `interactive = false` is provided by
    /// `capture::session` passing `is_repl = false`).
    #[tokio::test]
    async fn new_session_is_non_interactive() {
        let sess = session(Config::default());
        let guard = sess.session().lock().await;
        assert!(
            guard.prompter().is_none(),
            "MCP session must have no prompter"
        );
    }

    /// The happy path: `whoami` returns the same banner the REPL prints, routed
    /// through the shared engine.
    #[tokio::test]
    async fn run_command_whoami_returns_stdout() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        let sess = session(config);
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("whoami succeeds");
        assert!(out.starts_with("User: testuser, app pid: "), "got: {out:?}");
        assert!(out.ends_with('\n'), "trailing newline preserved: {out:?}");
    }

    /// An unknown flag is a parse failure: `McpCommandError` with exit 2 and the
    /// offending token surfaced in stderr.
    #[tokio::test]
    async fn run_command_argparse_failure_raises() {
        let sess = session(Config::default());
        let registry = register_all();

        let err = sess
            .run_command(&registry, "whoami", &["--bogus".to_owned()])
            .await
            .expect_err("unknown flag must fail");
        assert_eq!(err.exit_code, 2, "parse errors are argparse-exit-2");
        assert!(
            err.stderr.contains("bogus") || err.to_string().contains("bogus"),
            "stderr should name the bad flag: {err:?}"
        );
    }

    /// An unknown command maps to exit 1 (not a parse error).
    #[tokio::test]
    async fn run_command_unknown_command_raises_exit_1() {
        let sess = session(Config::default());
        let registry = register_all();

        let err = sess
            .run_command(&registry, "no_such_command", &[])
            .await
            .expect_err("unknown command must fail");
        assert_eq!(err.exit_code, 1);
    }

    /// `--help` is argparse-exit-0: it returns the help text as a success rather
    /// than an error envelope.
    #[tokio::test]
    async fn run_command_help_flag_is_success() {
        let sess = session(Config::default());
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &["--help".to_owned()])
            .await
            .expect("--help is a success");
        assert!(!out.is_empty(), "help text returned: {out:?}");
    }

    /// A tiny configured cap truncates the tool result and appends the notice.
    #[tokio::test]
    async fn run_command_output_is_capped() {
        let mut config = Config::default();
        config.session_user = "testuser".to_owned();
        config.mcp_max_output_bytes = 8; // far below the `whoami` banner length
        let sess = session(config);
        let registry = register_all();

        let out = sess
            .run_command(&registry, "whoami", &[])
            .await
            .expect("whoami succeeds");
        assert!(out.contains("truncated"), "cap notice present: {out:?}");
        assert!(
            out.contains("max_output_bytes=8"),
            "cap limit in notice: {out:?}"
        );
    }

    /// Each call isolates its own output: a second call does not see the first
    /// call's captured text.
    #[tokio::test]
    async fn run_command_isolates_output_per_call() {
        let mut config = Config::default();
        config.session_user = "alice".to_owned();
        let sess = session(config);
        let registry = register_all();

        let first = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        let second = sess.run_command(&registry, "whoami", &[]).await.unwrap();
        // Identical, single-banner output — not the first call's text doubled.
        assert_eq!(first, second);
        assert_eq!(
            second.matches("User: alice").count(),
            1,
            "no bleed: {second:?}"
        );
    }

    /// `McpCommandError` renders a one-line summary plus stderr.
    #[test]
    fn command_error_display() {
        let with_stderr = McpCommandError {
            stdout: String::new(),
            stderr: "unrecognized argument: --bogus".to_owned(),
            exit_code: 2,
        };
        assert_eq!(
            with_stderr.to_string(),
            "command failed (exit_code=2): unrecognized argument: --bogus"
        );

        let no_stderr = McpCommandError {
            stdout: String::new(),
            stderr: "   ".to_owned(),
            exit_code: 1,
        };
        assert_eq!(no_stderr.to_string(), "command failed (exit_code=1)");
    }
}
