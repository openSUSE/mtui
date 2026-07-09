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
//! The read loop and dispatch are independent of the editor's input features:
//! tab completion (P6.3), persistent history + Ctrl-R reverse-search + inline
//! hint (P6.4), and the workflow-aware prompt + RRID status + input highlighter
//! (P6.5) all live in the [`Reedline`] builder / [`MtuiPrompt`] in [`Repl::new`];
//! the command-timeout prompter (P6.6) slots in later without changing this loop.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use mtui_core::{EngineError, Registry, Session, dispatch_line};
use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, KeyCode, KeyModifiers, MenuBuilder, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};

use crate::completer::MtuiCompleter;
use crate::highlighter::MtuiHighlighter;
use crate::prompt::MtuiPrompt;

/// The reedline menu name the Tab keybinding activates.
const COMPLETION_MENU: &str = "completion_menu";

/// The banner printed once before the first prompt (upstream
/// `cmdloop(intro="Maintenance Test Update Installer")`).
const INTRO: &str = "Maintenance Test Update Installer";

/// The interactive REPL, owning the line editor and the command registry.
///
/// The registry and session are held behind [`Arc`]/[`Arc<Mutex>`] so the
/// [`MtuiCompleter`] (owned by the [`Reedline`] editor) can share them: reedline
/// hands the completer no session, so it reads the live one through the same
/// `Arc<Mutex<Session>>` this loop drives. Completion runs *during*
/// `read_line`; dispatch runs *after* it returns, so the two never hold the lock
/// at once.
pub struct Repl {
    line_editor: Reedline,
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
    prompt: MtuiPrompt,
}

impl Repl {
    /// Builds a REPL over `registry` and `session`, wiring tab completion,
    /// persistent history, and the inline history hint.
    ///
    /// The line editor is given an [`MtuiCompleter`] sharing `registry`/`session`
    /// plus a columnar completion menu bound to <kbd>Tab</kbd>; a
    /// [`file_backed_history`](crate::history::file_backed_history) persisting to
    /// `$XDG_DATA_HOME/mtui/history` (with Ctrl-R reverse-search from the default
    /// emacs bindings); and a [`DefaultHinter`] showing the greyed inline
    /// suggestion (upstream `AutoSuggestFromHistory`). The dynamic prompt/toolbar
    /// (P6.5) and the prompter (P6.6) slot into this builder later without
    /// changing the loop.
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<Mutex<Session>>) -> Self {
        let completer = Box::new(MtuiCompleter::new(
            Arc::clone(&registry),
            Arc::clone(&session),
        ));
        let highlighter = Box::new(MtuiHighlighter::new(
            Arc::clone(&registry),
            Arc::clone(&session),
        ));
        let menu = Box::new(ColumnarMenu::default().with_name(COMPLETION_MENU));

        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(COMPLETION_MENU.to_owned()),
                ReedlineEvent::MenuNext,
            ]),
        );
        let edit_mode = Box::new(Emacs::new(keybindings));

        let line_editor = Reedline::create()
            .with_completer(completer)
            .with_highlighter(highlighter)
            .with_menu(ReedlineMenu::EngineCompleter(menu))
            .with_edit_mode(edit_mode)
            .with_history(crate::history::file_backed_history())
            .with_hinter(Box::new(DefaultHinter::default()));

        let prompt = MtuiPrompt::new(Arc::clone(&session));

        Self {
            line_editor,
            registry,
            session,
            prompt,
        }
    }

    /// Runs the read → dispatch loop until `quit`/Ctrl-D, driving the session.
    ///
    /// # Errors
    ///
    /// Propagates a fatal editor I/O error from [`Reedline::read_line`] (e.g. a
    /// broken terminal). Command failures are *not* errors here: they are
    /// rendered to the session display and the loop continues.
    ///
    /// The session guard is held across `step`'s `.await`
    /// (`clippy::await_holding_lock`, allowed below). It is sound: this REPL runs
    /// on a current-thread `block_on`, and the editor's synchronous `read_line`
    /// (the only other lock holder, via the completer) has already returned
    /// before we lock. Nothing else contends — no host tasks are in flight
    /// mid-line — so the guard can never block another task at the await point. A
    /// `tokio::sync::Mutex` is the usual remedy, but its `blocking_lock` panics
    /// inside `read_line`'s runtime context and its async `lock` is unreachable
    /// from the synchronous completer, so the std `Mutex` + a scoped allow is the
    /// correct fit for this single-threaded editor↔dispatch bridge.
    #[allow(clippy::await_holding_lock)]
    pub async fn run(&mut self) -> anyhow::Result<()> {
        {
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            session.display.println(INTRO);
        }

        loop {
            match self.line_editor.read_line(&self.prompt)? {
                Signal::Success(line) => {
                    // Lock only for the dispatch; the completer's own lock during
                    // `read_line` was released before this returned. (Guard held
                    // across the await — justified on `run`'s doc comment.)
                    let should_break = {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        step(&self.registry, &mut session, &line).await.is_break()
                    };
                    if should_break {
                        break;
                    }
                }
                // Ctrl-C on a partial line: clear it and reprompt, never exit.
                Signal::CtrlC => {
                    let mut session = self
                        .session
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
