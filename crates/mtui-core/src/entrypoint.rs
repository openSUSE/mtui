//! Headless single-command dispatch entrypoint (`mtui-mcp` / embedding).
//!
//! This is the glue between the **three distinct argparse layers** mtui carries
//! (the correction that shaped P5.10 — do not conflate them):
//!
//! 1. **App invocation** — the top-level `mtui`/`mtui-mcp` process arguments,
//!    [`Args`](crate::args::Args) (port of upstream `mtui.cli.args.get_parser`).
//!    The real binary parses these with `Args::parse`, which exits the process
//!    on `--help`/`--version`/error exactly like upstream. This module takes an
//!    *already-parsed* `&Args`, so Layer 1 is the caller's responsibility (the
//!    binary is Phase 6).
//! 2. **REPL commands** — the per-command parsers the [`engine`](crate::engine)
//!    synthesises from the [`Registry`], run inside the REPL `cmdloop` and reused
//!    as MCP tools (port of upstream `mtui.commands._command.Command.parse_args`
//!    on the no-exit `ArgumentParser`). These never exit the process; they return
//!    a typed [`EngineError`].
//! 3. **MCP tool schema** — `mtui-mcp` translating each command's parser into
//!    JSON parameters (Phase 7). Not touched here.
//!
//! [`run_once`] dispatches exactly one Layer-2 command against a session and
//! yields a process [`ExitStatus`]: given the parsed top-level `Args` and one
//! command line, it resolves, parses, and runs a single command with no
//! interactive loop. It is the headless single-command primitive for
//! `mtui-mcp` (Phase 7) and embedding callers.
//!
//! It is **not** a CLI mode: upstream `mtui` — and the mtui `mtui` binary —
//! has only two surfaces, the interactive REPL and `mtui-mcp`, and neither takes
//! a positional command. The interactive binary seeds the session and enters the
//! REPL (`mtui-cli::seed_session` + `Repl`); it never calls `run_once`.
//!
//! ## Exit-code contract
//!
//! Upstream `run_mtui` collapses everything to `Literal[0, 1]`. mtui
//! **intentionally deviates**: it distinguishes clap/argparse's usage-error
//! convention (exit `2`) from a runtime failure (exit `1`), while keeping
//! `--help`/`--version` a success (exit `0`). See [`ExitStatus`].

use crate::args::Args;
use crate::engine::{EngineError, dispatch_line};
use crate::registry::Registry;
use crate::session::Session;

/// The process exit status a single non-interactive command run yields.
///
/// mtui deviates from upstream's collapsed `Literal[0, 1]` to preserve the
/// argparse/clap distinction between a *usage* error and a *runtime* failure:
///
/// * [`Ok`](ExitStatus::Ok) → `0` — the command ran, or clap printed
///   `--help`/`--version` (a success in argparse terms).
/// * [`Failure`](ExitStatus::Failure) → `1` — a runtime failure: unknown
///   command, unbalanced quotes, or the command body erroring.
/// * [`Usage`](ExitStatus::Usage) → `2` — a genuine argument *usage* error
///   (clap/argparse's exit-2 convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Success (or help/version output). Process exit code `0`.
    Ok,
    /// Runtime failure. Process exit code `1`.
    Failure,
    /// Argument usage error. Process exit code `2`.
    Usage,
}

impl ExitStatus {
    /// The numeric process exit code (`0`, `1`, or `2`).
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            ExitStatus::Ok => 0,
            ExitStatus::Failure => 1,
            ExitStatus::Usage => 2,
        }
    }
}

impl From<ExitStatus> for i32 {
    fn from(status: ExitStatus) -> Self {
        status.code()
    }
}

/// Runs exactly one command non-interactively and returns its [`ExitStatus`].
///
/// This is the headless single-command driver (consumed by `mtui-mcp` and
/// embedding callers, not the interactive CLI). It:
///
/// 1. dispatches `command_line` through the shared [`engine`](crate::engine)
///    (Layer 2 — resolve, parse, run), never exiting the process;
/// 2. on error, renders the message to the session display **once** (mirroring
///    upstream `run_mtui`'s `logger.error(e)`) — except for `--help`/`--version`
///    output, which clap already formatted for the user and which is a success;
/// 3. maps the outcome to a process [`ExitStatus`] per the contract above.
///
/// `args` is the already-parsed top-level [`Args`] (Layer 1, done by the
/// caller). Its `debug` flag is honoured here as a `tracing` breadcrumb so a
/// `-d` run leaves a trace of the dispatched line; the binary owns actual
/// log-level configuration.
///
/// A blank `command_line` is a no-op success ([`ExitStatus::Ok`]), matching the
/// engine's empty-line behaviour.
pub async fn run_once(
    registry: &Registry,
    session: &mut Session,
    args: &Args,
    command_line: &str,
) -> ExitStatus {
    if args.debug {
        tracing::debug!(command_line, "non-interactive dispatch");
    }

    match dispatch_line(registry, session, command_line).await {
        Ok(()) => ExitStatus::Ok,
        // `--help`/`--version`: clap already wrote the text; a success, no
        // extra rendering.
        Err(EngineError::Parse {
            help_or_version: true,
            ..
        }) => ExitStatus::Ok,
        // A genuine usage error: render it once, exit 2 (argparse convention).
        Err(e @ EngineError::Parse { .. }) => {
            render_error(session, &e);
            ExitStatus::Usage
        }
        // Any other failure (unknown command, syntax, command body): render
        // once, exit 1.
        Err(e) => {
            render_error(session, &e);
            ExitStatus::Failure
        }
    }
}

/// Renders a dispatch error to the session display exactly once as
/// `error: <message>`, the `error` token colorized red (upstream's
/// lowercased-red levelname + `": "`).
///
/// This is the **headless** rendering path (`mtui-mcp` / embedding). Unlike the
/// interactive REPL — which routes errors through `tracing::error!` so `error`,
/// `warn`, and `info` share one operator log channel — the headless entrypoint
/// has no `tracing` subscriber and must capture output through the session
/// display buffer. The two present the same `error: <message>` *text*; each uses
/// the channel appropriate to its surface. No `tracing` event is emitted here so
/// the failure never surfaces twice.
///
/// One exception: a [`EngineError::Parse`] here is always a genuine usage
/// error (the `--help`/`--version` case is filtered out in [`run_once`]
/// before `render_error` is ever called with it) and its message already
/// carries clap's own `error: ` prefix (and whatever color clap itself
/// applied). It is printed verbatim rather than wrapped a second time.
fn render_error(session: &mut Session, err: &EngineError) {
    match err {
        EngineError::Parse { message, .. } => session.display.println(message),
        _ => {
            let level = session.display.red("error");
            session.display.println(&format!("{level}: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Scope};
    use crate::display::{ColorMode, CommandPromptDisplay};
    use crate::error::{CommandError, CommandResult};
    use async_trait::async_trait;
    use clap::ArgMatches;
    use mtui_config::Config;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A command that runs successfully, or fails on demand.
    struct EchoCmd {
        runs: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait]
    impl Command for EchoCmd {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["e"]
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn configure(&self, cmd: clap::Command) -> clap::Command {
            cmd.arg(clap::Arg::new("word").num_args(0..=1))
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(CommandError::Other("boom".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    /// Default top-level args (nothing overridden).
    fn args() -> Args {
        Args {
            template_dir: None,
            sut: Vec::new(),
            connection_timeout: None,
            reboot_timeout: None,
            reboot_retries: None,
            debug: false,
            config: None,
            color: crate::args::ColorArg::Never,
            gitea_token: None,
            ssl_verify: None,
            auto_review_id: None,
            kernel_review_id: None,
        }
    }

    /// Builds a session whose display writes into a shared buffer, plus the
    /// handle to inspect what was rendered.
    fn session_with_buffer() -> (Session, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let display = CommandPromptDisplay::with_sink(
            Box::new(SharedBuf(Arc::clone(&buf))),
            ColorMode::Never,
        );
        let session = Session::with_display(Config::default(), false, display);
        (session, buf)
    }

    /// A `Write` sink backed by a shared buffer so a test can read the output.
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rendered(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn registry_with(fail: bool) -> (Registry, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(EchoCmd {
            runs: Arc::clone(&runs),
            fail,
        }));
        (r, runs)
    }

    #[tokio::test]
    async fn success_is_exit_zero_and_renders_nothing() {
        let (r, runs) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "echo hi").await;
        assert_eq!(status, ExitStatus::Ok);
        assert_eq!(status.code(), 0);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(
            rendered(&buf).is_empty(),
            "success must not render an error"
        );
    }

    #[tokio::test]
    async fn blank_line_is_exit_zero_noop() {
        let (r, runs) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "   ").await;
        assert_eq!(status, ExitStatus::Ok);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(rendered(&buf).is_empty());
    }

    #[tokio::test]
    async fn help_is_exit_zero_and_renders_no_error() {
        let (r, _) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "echo --help").await;
        assert_eq!(status, ExitStatus::Ok);
        // clap already wrote the help text; run_once must not render an *error*.
        assert!(
            rendered(&buf).is_empty(),
            "help/version must not be rendered as an error"
        );
    }

    #[tokio::test]
    async fn usage_error_is_exit_two_and_rendered_once() {
        let (r, runs) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "echo --no-such-flag").await;
        assert_eq!(status, ExitStatus::Usage);
        assert_eq!(status.code(), 2);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let out = rendered(&buf);
        // clap's usage text is multi-line; the "rendered once" contract means a
        // single `println`, so the usage marker appears exactly once.
        assert_eq!(
            out.matches("Usage:").count(),
            1,
            "usage error must be rendered exactly once, got: {out:?}"
        );
        // clap's own "error: " prefix must survive exactly once, not doubled
        // with mtui's own level prefix.
        assert_eq!(
            out.matches("error: ").count(),
            1,
            "exactly one error prefix, got: {out:?}"
        );
        assert!(!out.contains("error: error:"), "no doubled prefix: {out:?}");
    }

    #[tokio::test]
    async fn unknown_command_is_exit_one_and_rendered_once() {
        let (r, _) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "nope").await;
        assert_eq!(status, ExitStatus::Failure);
        assert_eq!(status.code(), 1);
        let out = rendered(&buf);
        assert!(out.contains("Unknown command"));
        assert_eq!(out.matches('\n').count(), 1, "rendered exactly once");
    }

    #[tokio::test]
    async fn syntax_error_is_exit_one() {
        let (r, _) = registry_with(false);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "echo \"unterminated").await;
        assert_eq!(status, ExitStatus::Failure);
        assert!(rendered(&buf).contains("invalid syntax"));
    }

    #[tokio::test]
    async fn command_body_failure_is_exit_one() {
        let (r, runs) = registry_with(true);
        let (mut s, buf) = session_with_buffer();
        let status = run_once(&r, &mut s, &args(), "echo hi").await;
        assert_eq!(status, ExitStatus::Failure);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the body ran, then failed");
        assert!(rendered(&buf).contains("boom"));
    }

    /// Headless `run_once` presents a failure as the same single, de-noised
    /// `error: <message>` line the REPL does (mtui-rs-7h9 shared contract): one
    /// line, `error: ` prefix, no `tracing` target / timestamp / `err=` noise.
    #[tokio::test]
    async fn error_line_matches_repl_prefix_and_is_denoised() {
        let (r, _) = registry_with(true);
        let (mut s, buf) = session_with_buffer();
        let _ = run_once(&r, &mut s, &args(), "echo hi").await;
        let out = rendered(&buf);
        assert_eq!(out.matches('\n').count(), 1, "rendered exactly once");
        assert!(out.starts_with("error: "), "upstream prefix, got: {out:?}");
        assert!(out.contains("boom"), "message present, got: {out:?}");
        assert!(!out.contains("command failed"), "no tracing message");
        assert!(!out.contains("err="), "no structured field noise");
    }

    #[tokio::test]
    async fn debug_flag_does_not_change_outcome() {
        let (r, _) = registry_with(false);
        let (mut s, _buf) = session_with_buffer();
        let mut a = args();
        a.debug = true;
        let status = run_once(&r, &mut s, &a, "echo hi").await;
        assert_eq!(status, ExitStatus::Ok);
    }

    #[test]
    fn exit_status_maps_to_i32() {
        assert_eq!(i32::from(ExitStatus::Ok), 0);
        assert_eq!(i32::from(ExitStatus::Failure), 1);
        assert_eq!(i32::from(ExitStatus::Usage), 2);
    }
}
