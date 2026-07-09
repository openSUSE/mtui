//! The interactive REPL: read → dispatch → repeat.
//!
//! Ports upstream `mtui.cli.repl.CommandPrompt.cmdloop` onto the Phase-5 engine.
//! Every line is handed to [`mtui_core::dispatch_line`] (the *same* engine the
//! MCP surface dispatches through), whose typed [`EngineError`] the loop renders
//! and then keeps going — a bad command never tears down the session, matching
//! upstream's `logger.error(e)` + continue.
//!
//! Control keys map onto [`reedline::Signal`]:
//!
//! * `Signal::Success(line)` → dispatch the line (empty lines are a no-op in the
//!   engine), render any error, then honour a pending `quit`
//!   ([`Session::should_exit`]).
//! * `Signal::CtrlC` → abort the current input and reprompt (upstream Ctrl-C on
//!   a partial line clears it); never exits.
//! * `Signal::CtrlD` → graceful session exit (upstream Ctrl-D → `EOF` alias of
//!   `quit`): break the loop, process exit 0.
//!
//! P6.2 scope: the read loop and dispatch only. Tab completion (P6.3), history +
//! reverse-search (P6.4), the workflow-aware prompt/toolbar (P6.5), and the
//! command-timeout prompter (P6.6) slot into the [`Reedline`] builder / the
//! [`MtuiPrompt`] later without changing this loop.

use std::ops::ControlFlow;

use mtui_core::{EngineError, Registry, Session, dispatch_line};
use reedline::{Reedline, Signal};

use crate::prompt::MtuiPrompt;

/// The banner printed once before the first prompt (upstream
/// `cmdloop(intro="Maintenance Test Update Installer")`).
const INTRO: &str = "Maintenance Test Update Installer";

/// The interactive REPL, owning the line editor and the command registry.
pub struct Repl {
    line_editor: Reedline,
    registry: Registry,
    prompt: MtuiPrompt,
}

impl Repl {
    /// Builds a REPL over `registry` with a bare line editor.
    ///
    /// The editor carries no completer/history/highlighter yet — those are
    /// P6.3/P6.4, added to this builder later.
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            line_editor: Reedline::create(),
            registry,
            prompt: MtuiPrompt,
        }
    }

    /// Runs the read → dispatch loop until `quit`/Ctrl-D, driving `session`.
    ///
    /// # Errors
    ///
    /// Propagates a fatal editor I/O error from [`Reedline::read_line`] (e.g. a
    /// broken terminal). Command failures are *not* errors here: they are
    /// rendered to the session display and the loop continues.
    pub async fn run(&mut self, session: &mut Session) -> anyhow::Result<()> {
        session.display.println(INTRO);

        loop {
            match self.line_editor.read_line(&self.prompt)? {
                Signal::Success(line) => {
                    if step(&self.registry, session, &line).await.is_break() {
                        break;
                    }
                }
                // Ctrl-C on a partial line: clear it and reprompt, never exit.
                Signal::CtrlC => {
                    session.display.println("");
                }
                // Ctrl-D: graceful session exit (upstream EOF → quit).
                Signal::CtrlD => break,
                // `#[non_exhaustive]`: any future/host signal is ignored and we
                // simply reprompt rather than crashing the session.
                _ => {}
            }
        }

        Ok(())
    }
}

/// Dispatches one input `line` and reports whether the loop should stop.
///
/// This is the testable heart of the loop, deliberately independent of the
/// TTY-bound [`Reedline`] editor: it dispatches through the shared engine,
/// renders any error to the session display exactly once, and reports
/// [`ControlFlow::Break`] iff the `quit` command asked the session to exit.
///
/// An empty/whitespace line is a no-op ([`ControlFlow::Continue`]) — the engine
/// already treats it as such.
pub async fn step(registry: &Registry, session: &mut Session, line: &str) -> ControlFlow<()> {
    if let Err(err) = dispatch_line(registry, session, line).await {
        render_error(session, &err);
    }
    if session.should_exit() {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

/// Renders a dispatch error to the session display exactly once (upstream
/// `logger.error(e)`), and mirrors it to `tracing` for the log sink.
///
/// Mirrors [`mtui_core::entrypoint::run_once`]'s error rendering so the REPL and
/// the non-interactive single-command mode present failures identically.
fn render_error(session: &mut Session, err: &EngineError) {
    let msg = err.to_string();
    tracing::error!(%err, "command failed");
    let line = session.display.red(&msg);
    session.display.println(&line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clap::ArgMatches;
    use mtui_config::Config;
    use mtui_core::command::{Command, Scope};
    use mtui_core::error::CommandResult;
    use mtui_core::{ColorMode, CommandPromptDisplay};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A command that counts its runs; on the deny-listed name `quit` it flips
    /// the session's exit flag (mirroring the real `Quit`).
    struct EchoCmd {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Command for EchoCmd {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn configure(&self, cmd: clap::Command) -> clap::Command {
            cmd.arg(clap::Arg::new("word").num_args(0..=1))
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A minimal `quit`: flips `request_exit`, like the real command.
    struct QuitCmd;

    #[async_trait]
    impl Command for QuitCmd {
        fn name(&self) -> &'static str {
            "quit"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            session.request_exit();
            Ok(())
        }
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

    fn session_with_buffer() -> (Session, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let display = CommandPromptDisplay::with_sink(
            Box::new(SharedBuf(Arc::clone(&buf))),
            ColorMode::Never,
        );
        (Session::with_display(Config::default(), true, display), buf)
    }

    fn rendered(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn registry() -> (Registry, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(EchoCmd {
            runs: Arc::clone(&runs),
        }));
        r.register(Arc::new(QuitCmd));
        (r, runs)
    }

    #[tokio::test]
    async fn known_command_runs_and_continues() {
        let (r, runs) = registry();
        let (mut s, buf) = session_with_buffer();
        let flow = step(&r, &mut s, "echo hi").await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(rendered(&buf).is_empty(), "success renders nothing");
    }

    #[tokio::test]
    async fn quit_breaks_the_loop() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let flow = step(&r, &mut s, "quit").await;
        assert_eq!(flow, ControlFlow::Break(()));
        assert!(s.should_exit());
    }

    #[tokio::test]
    async fn unknown_command_renders_error_and_continues() {
        let (r, runs) = registry();
        let (mut s, buf) = session_with_buffer();
        let flow = step(&r, &mut s, "nope").await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let out = rendered(&buf);
        assert!(out.contains("Unknown command"), "got: {out:?}");
        assert_eq!(out.matches('\n').count(), 1, "rendered exactly once");
    }

    #[tokio::test]
    async fn empty_line_is_a_noop_continue() {
        let (r, runs) = registry();
        let (mut s, buf) = session_with_buffer();
        let flow = step(&r, &mut s, "   ").await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(rendered(&buf).is_empty());
    }

    #[tokio::test]
    async fn bad_flag_renders_error_and_continues() {
        let (r, runs) = registry();
        let (mut s, buf) = session_with_buffer();
        let flow = step(&r, &mut s, "echo --no-such-flag").await;
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the body never ran");
        assert!(!rendered(&buf).is_empty(), "usage error is rendered");
    }
}
