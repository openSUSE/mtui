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
//! P7.3a (`mtui-rs-76e.11`) landed the per-template **lock discipline**: a
//! shared/exclusive registry gate ([`crate::concurrency::RwGate`], upstream
//! `_RWLock`) plus a lazily-created per-RRID lock map (upstream `_rrid_locks`).
//! [`command_lock`](McpSession::command_lock) takes the gate *shared* + one
//! per-RRID lock for a single-template call (so same-RRID calls serialise and
//! different-RRID calls take distinct locks) and the gate *exclusive* for
//! fan-out / registry mutators; [`scoped_lock`](McpSession::scoped_lock) is the
//! same hold for the hand-written testreport tools.
//!
//! **Not yet landed** — genuine wall-clock concurrency between *different-RRID*
//! calls (and per-call output isolation) additionally needs the `mtui-core`
//! change that stops dispatch taking `&mut Session` for the whole monolithic
//! session; until then different-RRID calls hold distinct per-RRID locks but
//! still serialise on the inner session `Mutex`. Tracked as `mtui-rs-f36r`
//! (the two `#[ignore]`d parity tests in `tests/session_concurrency.rs`).
//!
//! **Deferred** to their own follow-up beads (this type grows in place — do not
//! replace it):
//!   - the background-job table (`_jobs`) + job tools — `mtui-rs-76e.12`;
//!   - `close()` host teardown (disconnect every loaded template's hosts,
//!     release pool claims), owned by the http idle sweep — `mtui-rs-76e.13`;
//!   - `notifications/progress` heartbeats for long calls — `mtui-rs-76e.14`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use mtui_config::Config;
use mtui_core::{EngineError, Registry, Session, dispatch_argv, resolve_command_rrids};
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::capture::{self, SharedBuf};
use crate::concurrency::{ExclusiveGuard, RwGate, SharedGuard};
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
    /// The registry shared/exclusive gate (upstream `_RWLock` `_registry`).
    ///
    /// A command scoped to exactly one template enters this in *shared* mode
    /// (so it cannot overlap a registry mutation); registry mutators
    /// (`load_template`/`unload`) and unscoped fan-out enter it *exclusive*,
    /// draining in-flight per-RRID work. See [`command_lock`](Self::command_lock).
    gate: RwGate,
    /// Lazily-created per-RRID locks (upstream `_rrid_locks` + `_locks_guard`).
    ///
    /// Same-RRID calls share one `Arc<Mutex<()>>` and serialise; different-RRID
    /// calls take different locks. The outer [`StdMutex`] guards the map's own
    /// lazy population (held only for the get-or-insert, never across an await).
    rrid_locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// An acquired hold on the concurrency gate for one command/tool invocation.
///
/// Returned by [`McpSession::command_lock`] / [`McpSession::scoped_lock`] and
/// kept alive for the duration of the critical section; dropping it releases the
/// gate (and any per-RRID lock) in the right order. The fields are never read —
/// they exist to own the guards — hence the leading underscores.
#[must_use = "dropping the CommandLock immediately releases the gate"]
pub enum CommandLock {
    /// A single-template hold: the registry gate shared **plus** one per-RRID
    /// lock. The `_rrid` guard drops first (declaration order), then `_shared`,
    /// matching the acquire order (gate-shared → rrid lock) in reverse.
    Scoped {
        /// The per-RRID lock (dropped first).
        _rrid: OwnedMutexGuard<()>,
        /// The registry gate held in shared mode (dropped second).
        _shared: SharedGuard,
    },
    /// A registry-wide exclusive hold (mutators / unscoped fan-out).
    Exclusive(#[allow(dead_code)] ExclusiveGuard),
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
            gate: RwGate::new(),
            rrid_locks: StdMutex::new(HashMap::new()),
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

    /// Returns (creating on first use) the per-template lock for `rrid`.
    ///
    /// Lazily populates [`rrid_locks`](Self::rrid_locks) under its guard so two
    /// tasks racing to lock the same fresh RRID share one lock object. The Rust
    /// analogue of upstream `_lock_for`.
    fn lock_for(&self, rrid: &str) -> Arc<Mutex<()>> {
        let mut map = self.rrid_locks.lock().expect("rrid lock map poisoned");
        Arc::clone(
            map.entry(rrid.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Acquires the right lock(s) for a `name`/`argv` invocation and returns a
    /// guard holding them for the caller's critical section.
    ///
    /// The Rust analogue of upstream `_command_lock`, resolving exactly as the
    /// foreground dispatch does (via [`resolve_command_rrids`]):
    ///
    /// * resolves to **exactly one** loaded template → the registry gate in
    ///   *shared* mode **plus** that template's per-RRID lock, so different-RRID
    ///   commands run concurrently while same-RRID commands serialise and no
    ///   command overlaps a registry mutation;
    /// * fan-out / unscoped-multi commands, registry mutators
    ///   (`load_template`/`unload`), or anything that resolves to no real
    ///   template → the registry gate in *exclusive* mode, which drains in-flight
    ///   per-RRID commands and blocks new ones for the duration.
    ///
    /// A single call never holds two per-RRID locks and the exclusive path holds
    /// only the gate, so the lock order (gate-shared → one rrid lock) is total
    /// and cannot deadlock. Resolution needs the [`Session`] (loaded set + active
    /// pointer), so it briefly locks the session — released before the returned
    /// guard is handed back, so the caller may re-lock the session for dispatch.
    async fn command_lock(&self, registry: &Registry, name: &str, argv: &[String]) -> CommandLock {
        let rrids = match registry.get(name) {
            Some(command) => {
                let session = self.session.lock().await;
                resolve_command_rrids(command.as_ref(), &session, argv)
            }
            // Unknown command: no meaningful scope, serialise conservatively.
            None => None,
        };

        match rrids {
            Some(rrids) if rrids.len() == 1 => {
                let shared = self.gate.shared().await;
                let lock = self.lock_for(&rrids[0]);
                let rrid = lock.lock_owned().await;
                CommandLock::Scoped {
                    _shared: shared,
                    _rrid: rrid,
                }
            }
            _ => CommandLock::Exclusive(self.gate.exclusive().await),
        }
    }

    /// Holds the registry-shared gate plus one template's per-RRID lock.
    ///
    /// For the hand-written testreport tools (which act on a single template's
    /// files): entering the gate *shared* keeps the loaded set stable for the
    /// body (no concurrent `load_template`/`unload`) while still letting tools on
    /// *other* templates run in parallel, and the per-RRID lock serialises
    /// against foreground dispatch for the *same* template (e.g. a concurrent
    /// `commit`). The Rust analogue of upstream `scoped_lock`.
    ///
    /// `rrid` is the resolved target template id, or `None` to fall back to the
    /// active template (single-/zero-loaded case). Callers should resolve and
    /// validate the target report *inside* the body, where the shared gate
    /// guarantees the registry cannot change underfoot.
    pub async fn scoped_lock(&self, rrid: Option<&str>) -> CommandLock {
        let shared = self.gate.shared().await;
        let key = match rrid {
            Some(r) => r.to_owned(),
            None => self
                .session
                .lock()
                .await
                .templates
                .active_rrid()
                .unwrap_or("")
                .to_owned(),
        };
        let lock = self.lock_for(&key);
        let rrid = lock.lock_owned().await;
        CommandLock::Scoped {
            _shared: shared,
            _rrid: rrid,
        }
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
    /// Before dispatch the call takes its [`command_lock`](Self::command_lock):
    /// a single-template call holds the registry gate *shared* plus its per-RRID
    /// lock (so same-RRID calls serialise, different-RRID calls take distinct
    /// locks), while fan-out / mutators take the gate *exclusive*. The dispatch
    /// itself still holds the single session `Mutex` (the `mtui-core` change that
    /// lets different-RRID dispatch run truly in parallel is `mtui-rs-f36r`). A
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
        // Acquire the per-template / registry-gate hold for this invocation
        // *before* touching the session, so same-RRID and unscoped calls
        // serialise and mutators drain in-flight per-RRID work. Held for the
        // whole dispatch, released when `_lock` drops at end of scope.
        let _lock = self.command_lock(registry, name, argv).await;

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

    /// `lock_for` returns the *same* lock object for a repeated RRID (so
    /// same-RRID calls contend) and a *different* one for a distinct RRID.
    #[test]
    fn lock_for_shares_per_rrid() {
        let sess = session(Config::default());
        let a1 = sess.lock_for("SUSE:Maintenance:1:1");
        let a2 = sess.lock_for("SUSE:Maintenance:1:1");
        let b = sess.lock_for("SUSE:Maintenance:2:1");
        assert!(Arc::ptr_eq(&a1, &a2), "same RRID shares one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "distinct RRIDs get distinct locks");
    }

    /// An unknown command resolves to no RRID → `command_lock` takes the gate
    /// exclusively (upstream unscoped fallback).
    #[tokio::test]
    async fn command_lock_unknown_is_exclusive() {
        let sess = session(Config::default());
        let registry = register_all();
        let lock = sess.command_lock(&registry, "no_such_command", &[]).await;
        assert!(
            matches!(lock, CommandLock::Exclusive(_)),
            "unknown command serialises exclusively"
        );
    }

    /// A self-scoped single-shot command with nothing loaded resolves to the
    /// null report only → exclusive gate (unscoped fallback).
    #[tokio::test]
    async fn command_lock_unscoped_is_exclusive() {
        let sess = session(Config::default());
        let registry = register_all();
        // `whoami` is `Scope::Active`; with nothing loaded it resolves to the
        // empty null RRID, which `resolve_command_rrids` drops → None → exclusive.
        let lock = sess.command_lock(&registry, "whoami", &[]).await;
        assert!(matches!(lock, CommandLock::Exclusive(_)));
    }

    /// `scoped_lock(None)` with nothing loaded falls back to the active (empty)
    /// RRID and yields a scoped hold without deadlocking.
    #[tokio::test]
    async fn scoped_lock_falls_back_to_active() {
        let sess = session(Config::default());
        let lock = sess.scoped_lock(None).await;
        assert!(matches!(lock, CommandLock::Scoped { .. }));
    }
}
