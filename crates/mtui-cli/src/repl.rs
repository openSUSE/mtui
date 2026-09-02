//! The interactive REPL: read → dispatch → repeat.
//!
//! Every line goes to [`mtui_core::dispatch_line`] — the *same* engine the MCP
//! surface dispatches through — whose typed [`EngineError`] the loop renders
//! before carrying on, so a bad command never tears down the session.
//!
//! On [`reedline::Signal`]: `Success(line)` dispatches, then honours a pending
//! `quit` ([`Session::should_exit`]); `CtrlD` dispatches `quit`'s `EOF` alias so
//! the full teardown runs; `CtrlC` clears a partial line and reprompts, because
//! reedline holds raw mode while reading and Ctrl-C is a mere key event there.
//!
//! *While a command runs* the terminal is cooked and Ctrl-C is a real SIGINT,
//! forwarded onto the session's cancellation seam (`spawn_interrupt_forwarder`
//! → `step_interruptible`) rather than killing the process, which skipped every
//! teardown and stranded a dead-pid operation lock on each locked host. The
//! first press cancels at the next checkpoint, the second force-quits with a
//! record of the locks left behind. During the teardown — Ctrl-D *or* a typed
//! `quit`/`exit` — presses escalate but never *cancel*, since cancelling the
//! cleanup is what strands the locks (`OnPress`).
//!
//! Tab completion, persistent history + Ctrl-R + inline hint, and the
//! workflow-aware prompt/highlighter live in the [`Reedline`] builder /
//! [`MtuiPrompt`] in [`Repl::new`]; the command-timeout prompter is wired at the
//! composition root, `run()` in `lib.rs`.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use mtui_core::{EngineError, ExitStatus, Registry, Session, dispatch_line};
use reedline::{
    ColumnarMenu, DefaultHinter, Emacs, KeyCode, KeyModifiers, MenuBuilder, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::completer::MtuiCompleter;
use crate::highlighter::MtuiHighlighter;
use crate::prompt::MtuiPrompt;

/// The reedline menu name the Tab keybinding activates.
const COMPLETION_MENU: &str = "completion_menu";

/// The banner printed once before the first prompt.
const INTRO: &str = "Maintenance Test Update Installer";

/// How many unread Ctrl-C presses the forwarder queues before coalescing: one
/// to cancel, one to force-quit, and a third says nothing the second did not.
const INTERRUPT_QUEUE: usize = 2;

/// The command whose dispatch *is* the session teardown.
///
/// Matched after registry resolution, so its aliases (`exit`, `EOF`) — and any
/// added later — come along for free.
const QUIT_COMMAND: &str = "quit";

/// How [`Repl::run`] ended.
///
/// A force-quit is *decided* here and *executed* by the caller: reedline
/// persists its `FileBackedHistory` on drop and `std::process::exit` runs no
/// destructors, so the process must outlive the line editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplExit {
    /// `quit`/Ctrl-D: the teardown ran, the process exits normally.
    Normal,
    /// A double Ctrl-C: the caller flushes the history and exits with
    /// [`ReplExit::status`].
    ForceQuit,
}

impl ReplExit {
    /// The process status to exit with, or `None` to return from `main`
    /// normally. [`ExitStatus::Interrupted`] is 128 + `SIGINT`, the shell
    /// convention for a Ctrl-C death, routed through [`ExitStatus`] rather than
    /// a bare integer so the binary speaks one exit vocabulary.
    #[must_use]
    pub fn status(self) -> Option<ExitStatus> {
        match self {
            Self::Normal => None,
            Self::ForceQuit => Some(ExitStatus::Interrupted),
        }
    }
}

/// What a Ctrl-C press does to the dispatch it lands on: the one difference
/// between an ordinary command and the session teardown, so they can share
/// [`step_interruptible`] rather than keeping two subtle `select!` loops in
/// sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnPress {
    /// An ordinary command: the first press cancels it at its next checkpoint.
    Cancel,
    /// The `quit` teardown, however it was asked for (Ctrl-D or a typed
    /// `quit`/`exit`): a press **never** cancels, because cancelling the
    /// cleanup is what strands the locks. It still counts toward the
    /// force-quit, because the teardown can genuinely hang — `quit`'s
    /// pool-claim release has no timeout of its own.
    EscalateOnly,
}

impl OnPress {
    /// The notice the *first* press prints. Informational, so `warn`: an
    /// operator who set `error` has said they do not want it, whereas
    /// [`on_escalate`]'s record is not suppressible.
    fn first_press_notice(self) -> &'static str {
        match self {
            Self::Cancel => {
                "cancelling — the command stops at its next checkpoint (a host operation already \
                 under way finishes first); press Ctrl-C again to force-quit, which may strand \
                 operation locks"
            }
            Self::EscalateOnly => {
                "teardown in progress (releasing pool claims, closing hosts) — it cannot be \
                 cancelled; press Ctrl-C again to force-quit, which may leave pool claims and \
                 operation locks behind"
            }
        }
    }
}

/// Records the force-quit and reports the ending.
///
/// The two kinds name different remedies because they abandon different things:
/// a command its operation locks, a teardown the pool claims as well. Extracted
/// from the loop's arms so that pairing is testable — the arms need a terminal.
/// `error!`, not `warn!`: the only record that mtui is walking away from locks
/// it holds must survive `set_log_level error`.
fn on_escalate(on_press: OnPress) -> ReplExit {
    match on_press {
        OnPress::Cancel => tracing::error!(
            "forcing exit mid-command; operation locks may remain on the update's hosts — \
             release them with `unlock --force` from a new session"
        ),
        OnPress::EscalateOnly => tracing::error!(
            "forcing exit mid-teardown; pool claims and operation locks may remain on the \
             update's hosts — release them with `unlock --force` and `unlock --pool` from a new \
             session"
        ),
    }
    ReplExit::ForceQuit
}

/// Decides what a press must do to the dispatch of `line`.
///
/// A typed `quit`/`exit`/`EOF` *is* the teardown Ctrl-D dispatches, so it gets
/// the teardown's rules: otherwise typing `quit` at a blackholed refhost cancels
/// one's own cleanup, and the advice that follows never mentions the stranded
/// pool claim. The first token is resolved through the registry (the
/// [`is_shell_line`](crate::shell::is_shell_line) precedent), so only a
/// command-position hit counts and `help quit` stays a `help` line; an
/// unparseable line takes the ordinary path and the engine renders its error.
fn press_policy(registry: &Registry, line: &str) -> OnPress {
    let Some(tokens) = shlex::split(line) else {
        return OnPress::Cancel;
    };
    let quits = tokens
        .first()
        .and_then(|name| registry.get(name))
        .is_some_and(|cmd| cmd.name() == QUIT_COMMAND);
    if quits {
        OnPress::EscalateOnly
    } else {
        OnPress::Cancel
    }
}

/// The interactive REPL, owning the line editor and the command registry.
///
/// The registry and session sit behind [`Arc`]/[`Arc<Mutex>`] because reedline
/// hands the [`MtuiCompleter`] it owns no session, so the completer reads the
/// live one through the same handle this loop drives. Completion runs *during*
/// `read_line` and dispatch *after* it returns, so neither holds the lock at
/// once.
pub struct Repl {
    line_editor: Reedline,
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
    prompt: MtuiPrompt,
}

impl Repl {
    /// Builds a REPL over `registry` and `session`: an [`MtuiCompleter`] behind a
    /// columnar menu bound to <kbd>Tab</kbd>, a `file_backed_history` persisting
    /// to `$XDG_DATA_HOME/mtui/history` (Ctrl-R comes from the default emacs
    /// bindings), a [`DefaultHinter`], and [`MtuiPrompt`]. The command-timeout
    /// prompter is wired separately at the composition root, `run()` in `lib.rs`.
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

    /// Consumes the REPL, returning **only** the line editor; the rest is
    /// leaked, not dropped.
    ///
    /// The force-quit path's contract is "run exactly one destructor, then exit",
    /// which neither `process::exit` (runs none) nor returning from `main` (runs
    /// all, and blocks on the runtime) expresses; the one that must run is
    /// reedline's `FileBackedHistory` flush. Since `Repl` solely owns the
    /// `Arc<Mutex<Session>>`, dropping the rest would synchronously tear down
    /// every SSH `Target`, HTTP client and template on the one path whose job is
    /// to get out now — nothing there blocks today, and leaking keeps it so.
    #[must_use]
    pub fn into_line_editor(self) -> Reedline {
        let Self {
            line_editor,
            registry,
            session,
            prompt,
        } = self;
        std::mem::forget((registry, session, prompt));
        line_editor
    }

    /// Runs the read → dispatch loop until `quit`/Ctrl-D, driving the session.
    ///
    /// Returns [`ReplExit::Normal`], or [`ReplExit::ForceQuit`] when a double
    /// Ctrl-C asked to stop waiting for a command (or a teardown); the caller
    /// executes that decision — see [`ReplExit`] for why not here.
    ///
    /// # Errors
    ///
    /// Propagates a fatal editor I/O error from [`Reedline::read_line`]. Command
    /// failures are *not* errors here: they are rendered and the loop continues.
    ///
    /// The session guard is held across the dispatch's `.await`
    /// (`clippy::await_holding_lock`, allowed below). Sound because nothing else
    /// can want the lock there: the only other holder is the synchronous
    /// `read_line`, which returned before we lock, and the interrupt forwarder
    /// touches no session state. A `tokio::sync::Mutex` is the usual remedy, but
    /// its `blocking_lock` panics inside `read_line`'s runtime context and its
    /// async `lock` is unreachable from the synchronous completer, so std
    /// `Mutex` + a scoped allow fits this alternating editor↔dispatch bridge.
    #[allow(clippy::await_holding_lock)]
    pub async fn run(&mut self) -> anyhow::Result<ReplExit> {
        {
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            session.display.println(INTRO);
        }
        // Armed before the first prompt, for the whole session — see the
        // forwarder's own note on why that is what makes Ctrl-C deterministic.
        let mut interrupts = spawn_interrupt_forwarder();

        loop {
            match self.line_editor.read_line(&self.prompt)? {
                Signal::Success(line) => {
                    // `shell` attaches an interactive PTY, needing the local TTY
                    // only this REPL owns; the engine shared with headless MCP
                    // has a stub, so the line is intercepted before dispatch.
                    if let Some(argv) = crate::shell::is_shell_line(&line) {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Err(e) = crate::shell::run_shell(&mut session, &argv).await {
                            tracing::error!("{e}");
                        }
                        continue;
                    }
                    // Same for `edit`, which foregrounds `$EDITOR` on the
                    // controlling TTY for its lifetime.
                    if let Some(argv) = crate::edit::is_edit_line(&line) {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Err(e) = crate::edit::run_edit(&mut session, &argv) {
                            tracing::error!("{e}");
                        }
                        continue;
                    }
                    let on_press = press_policy(&self.registry, &line);
                    // Guard held across the await — justified on `run`'s doc.
                    let outcome = {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        step_interruptible(
                            &self.registry,
                            &mut session,
                            &line,
                            &mut interrupts,
                            on_press,
                        )
                        .await
                    };
                    match outcome {
                        StepOutcome::Flow(flow) => {
                            if flow.is_break() {
                                break;
                            }
                        }
                        // A second Ctrl-C: the operator declined to wait for the
                        // cooperative stop. Loudly, since this is the one path
                        // that *can* leave locks a clean stop would release.
                        StepOutcome::Escalate => return Ok(on_escalate(on_press)),
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
                // Ctrl-D dispatches the `EOF` alias through the engine so the
                // full teardown runs (pool-claim release + host close); a bare
                // `break` would skip it. reedline persists the history when the
                // editor drops after `run` returns.
                Signal::CtrlD => {
                    let outcome = {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        step_interruptible(
                            &self.registry,
                            &mut session,
                            "EOF",
                            &mut interrupts,
                            OnPress::EscalateOnly,
                        )
                        .await
                    };
                    // Same escape hatch as mid-command; whatever the teardown
                    // had not reached stays claimed/locked.
                    if outcome == StepOutcome::Escalate {
                        return Ok(on_escalate(OnPress::EscalateOnly));
                    }
                    break;
                }
                // `#[non_exhaustive]`: reprompt on any future signal rather
                // than crashing the session.
                _ => {}
            }
        }

        Ok(ReplExit::Normal)
    }
}

/// Spawns the SIGINT forwarder and hands back the channel the dispatch loop
/// reads presses from.
///
/// Spawned once per [`Repl::run`] and outliving every dispatch: the SIGINT
/// handler is installed *process-wide* on first use and never uninstalled, so
/// arming here — before the first prompt — is what makes Ctrl-C mean one thing
/// for the whole session, rather than depending on whether an earlier command
/// armed it as a side effect. Since reedline holds raw mode while `read_line`
/// owns the terminal, the forwarder only sees presses from a cooked window: a
/// running command, or a gap between them ([`step_interruptible`] drains those).
///
/// "The whole session" means *from the first prompt onward*: startup seeding
/// (`-a`/`-k`/`--sut`) runs before this and a Ctrl-C there still kills the
/// process. Arming earlier without a consumer would make Ctrl-C during a
/// 60-second connect a silent no-op, so the fix is to route the seeding through
/// this protocol, not to move this call. Only the wiring lives here; the effect
/// is [`step_interruptible`], tested by injecting on this same channel, since a
/// real `SIGINT` would take every other test in the shared binary down.
fn spawn_interrupt_forwarder() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(INTERRUPT_QUEUE);
    tokio::spawn(async move {
        // A full queue already holds an unread press (coalesce); a closed one
        // means the loop is gone and nothing will read again.
        let forward = |tx: &mpsc::Sender<()>| {
            !matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(())))
        };
        // One long-lived stream, created *before* the first press: a fresh
        // `ctrl_c()` subscription per press only sees signals arriving after
        // its first poll, losing one that lands between two calls.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::interrupt()) {
                Ok(mut sigint) => {
                    while sigint.recv().await.is_some() {
                        if !forward(&tx) {
                            break;
                        }
                    }
                }
                // Uninstallable: SIGINT keeps its default (fatal) disposition
                // and retrying would only spin.
                Err(e) => {
                    tracing::debug!(error = %e, "no SIGINT handler; Ctrl-C stays fatal");
                }
            }
        }
        // No `SignalKind` off unix; `ctrl_c` is the portable equivalent, at the
        // cost of the re-subscribe window above.
        #[cfg(not(unix))]
        while tokio::signal::ctrl_c().await.is_ok() {
            if !forward(&tx) {
                break;
            }
        }
    });
    rx
}

/// What one interruptible dispatch decided. Split from the execution so the
/// decision is testable: [`Repl::run`] turns [`Escalate`](Self::Escalate) into a
/// process exit, which a test cannot observe without exiting with it.
#[derive(Debug, PartialEq, Eq)]
enum StepOutcome {
    /// The dispatch ran to completion (cancelled or not); carries [`step`]'s
    /// control flow.
    Flow(ControlFlow<()>),
    /// A second Ctrl-C arrived while the first cancel was still settling: stop
    /// waiting for the command and force-quit.
    Escalate,
}

/// Dispatches one input `line` with Ctrl-C wired to the cancellation seam.
///
/// The interruptible sibling of [`step`], and like it deliberately TTY-free:
/// presses arrive on `interrupts` — [`spawn_interrupt_forwarder`]'s channel in
/// the live REPL, a test's own sender otherwise — so the whole protocol is
/// exercisable without raising a signal. Three constraints shape the body:
///
/// * **A fresh token per dispatch, installed unconditionally.** A
///   [`CancellationToken`] is one-shot and this loop never resets it, so a
///   cancel would otherwise poison *every* later dispatch — the `quit`/`EOF`
///   teardown included, which would die at the `Command::run` pre-flight check
///   and strand exactly the pool claims and host locks a cooperative cancel
///   exists to release. Unconditional installation is the MCP job layer's
///   self-healing shape.
/// * **Stale presses are drained first.** A Ctrl-C from a cooked gap between
///   commands belongs to no dispatch and must not cancel the next one.
/// * **The dispatch future is never dropped on a press.** Cancelling the token
///   asks the flow to stop at its next checkpoint; dropping it would abandon
///   the flow mid-step, which is the teardown hole this replaces. Escalation is
///   the one exception, at the operator's explicit second request.
///
/// What the first press buys depends on the command: checkpointed flows
/// (`update`, `prepare`, `downgrade`, every fan-out boundary) stop promptly with
/// their locks released, while a host operation already under way (`run`,
/// `install`, `uninstall`) finishes first and unlocks normally.
///
/// `on_press` is the *only* difference between an ordinary command and the
/// teardown ([`OnPress::EscalateOnly`] skips the `token.cancel()` and prints the
/// teardown's notice). Sharing one body is deliberate: two copies of a
/// biased-select protocol drift, which is how a typed `quit` came to be
/// unprotected where Ctrl-D was not.
async fn step_interruptible(
    registry: &Registry,
    session: &mut Session,
    line: &str,
    interrupts: &mut mpsc::Receiver<()>,
    on_press: OnPress,
) -> StepOutcome {
    // Stale presses, then a token this dispatch owns (both per the contract
    // above).
    while interrupts.try_recv().is_ok() {}

    session.set_cancel_token(CancellationToken::new());
    // Cloned before `step` borrows the session mutably; clones share state.
    let token = session.cancel_token();

    let dispatch = step(registry, session, line);
    tokio::pin!(dispatch);
    let mut pressed = false;
    loop {
        let press = tokio::select! {
            // Biased, completion first: a press landing in the same poll
            // window as the command's own completion must not be attributed to
            // it, or the race force-quits a command that just finished cleanly
            // — warning about released locks, and skipping the `quit` teardown
            // that would release the pool claims. A press that loses the race
            // stays queued for the next dispatch to drain.
            biased;
            flow = &mut dispatch => return StepOutcome::Flow(flow),
            press = interrupts.recv() => press,
        };
        if press.is_none() {
            // The forwarder is gone (only when SIGINT cannot be handled at
            // all), so nothing can interrupt this dispatch: see it through
            // instead of spinning on a closed channel.
            return StepOutcome::Flow((&mut dispatch).await);
        }
        if pressed {
            return StepOutcome::Escalate;
        }
        pressed = true;
        if on_press == OnPress::Cancel {
            token.cancel();
        }
        // Signal path only, leaving the normal dispatch's
        // exactly-one-`error: `-line contract untouched.
        tracing::warn!("{}", on_press.first_press_notice());
    }
}

/// Dispatches one input `line` and reports whether the loop should stop.
///
/// The testable heart of the loop, deliberately independent of the TTY-bound
/// [`Reedline`] editor: it dispatches through the shared engine, reports any
/// error through `tracing::error!` exactly once, and reports
/// [`ControlFlow::Break`] iff `quit` asked the session to exit. An
/// empty/whitespace line is a no-op the engine already handles.
async fn step(registry: &Registry, session: &mut Session, line: &str) -> ControlFlow<()> {
    if let Err(err) = dispatch_line(registry, session, line).await {
        render_error(&err);
    }
    if session.should_exit() {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

/// Reports a dispatch error through `tracing::error!`, the single operator log
/// channel.
///
/// Errors, warnings and info share one path: the subscriber installed by
/// [`init_tracing`](crate::init_tracing), whose
/// [`CompactLevelFormat`](crate::logfmt::CompactLevelFormat) renders each level
/// as a lowercased, colorized token under one `--color` decision. The message is
/// the event's *message*, not a structured field, so no `err=` noise appears.
/// Headless `mtui-mcp` renders identical *text* through its captured display
/// buffer instead — same text, the channel each surface has.
///
/// One exception: a genuine usage error (`Parse { help_or_version: false, .. }`)
/// already carries clap's own `error: ` prefix, so it is tagged with
/// [`CLAP_PREFIXED_TARGET`](crate::logfmt::CLAP_PREFIXED_TARGET), which
/// `CompactLevelFormat` renders without adding a second one.
/// `--help`/`--version` output takes the normal path.
fn render_error(err: &EngineError) {
    match err {
        EngineError::Parse {
            help_or_version: false,
            message,
        } => {
            tracing::error!(target: crate::logfmt::CLAP_PREFIXED_TARGET, "{message}");
        }
        _ => tracing::error!("{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clap::ArgMatches;
    use mtui_config::Config;
    use mtui_core::command::{Command, Scope};
    use mtui_core::error::{CommandError, CommandResult};
    use mtui_core::{ColorMode, CommandPromptDisplay};
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// A generous upper bound for the interrupt tests. The work is in-process
    /// channel traffic, so overrunning it means a hang: a dispatch dropped,
    /// never woken, or awaiting a cancel that never came fails an assertion
    /// instead of wedging the binary. A busy-spin still hangs.
    const BOUND: Duration = Duration::from_secs(5);

    /// How many independent races [`a_press_racing_completion_never_force_quits`]
    /// runs. An unbiased select picks the interrupt branch about half the time,
    /// so surviving all 32 rounds has probability 2⁻³².
    const RACE_ROUNDS: usize = 32;

    /// A command that counts its runs.
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

    /// A command that parks on the cancellation seam and records that it was
    /// polled all the way to its own `return` afterwards — the observable proof
    /// the dispatch future was *not* dropped when the interrupt arrived.
    struct ParkCmd {
        started: Mutex<Option<oneshot::Sender<()>>>,
        finished: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Command for ParkCmd {
        fn name(&self) -> &'static str {
            "park"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            if let Some(tx) = self.started.lock().unwrap().take() {
                let _ = tx.send(());
            }
            session.cancel_token().cancelled().await;
            self.finished.store(true, Ordering::SeqCst);
            Err(CommandError::Cancelled(String::new()))
        }
    }

    /// A command that never observes the seam — the `run`/`install` shape, where
    /// a cancel is inert until the host operation finishes and only a second
    /// press gets the operator out.
    struct DeafCmd {
        started: Mutex<Option<oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl Command for DeafCmd {
        fn name(&self) -> &'static str {
            "deaf"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            if let Some(tx) = self.started.lock().unwrap().take() {
                let _ = tx.send(());
            }
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A command that waits to be let go, then reports whether the token was
    /// cancelled behind its back. Parking first stops a dispatch that finished
    /// before the loop looked at the channel from hiding a stale press.
    struct GateCmd {
        go: Mutex<Option<oneshot::Receiver<()>>>,
        observed_cancel: Arc<AtomicBool>,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Command for GateCmd {
        fn name(&self) -> &'static str {
            "gate"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            let token = session.cancel_token();
            let go = self.go.lock().unwrap().take().expect("gate armed once");
            let _ = go.await;
            self.observed_cancel
                .store(token.is_cancelled(), Ordering::SeqCst);
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A `quit` that takes its time — the real one's shape when a refhost
    /// blackholes, since its pool-claim release has no timeout. Records whether
    /// anything cancelled its token, which nothing on this path may do.
    struct SlowQuitCmd {
        go: Mutex<Option<oneshot::Receiver<()>>>,
        observed_cancel: Arc<AtomicBool>,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Command for SlowQuitCmd {
        fn name(&self) -> &'static str {
            "quit"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["exit", "EOF"]
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
            let token = session.cancel_token();
            let go = self.go.lock().unwrap().take().expect("gate armed once");
            let _ = go.await;
            self.observed_cancel
                .store(token.is_cancelled(), Ordering::SeqCst);
            self.runs.fetch_add(1, Ordering::SeqCst);
            session.request_exit();
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
        fn aliases(&self) -> &'static [&'static str] {
            &["exit", "EOF"]
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

    /// A `MakeWriter` over a shared buffer, so a scoped `tracing` subscriber's
    /// output (where `render_error` now sends the error) can be inspected.
    #[derive(Clone)]
    struct BufMaker(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufMaker {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            SharedBuf(Arc::clone(&self.0))
        }
    }

    /// Runs `step` on `line` under the REPL's real [`CompactLevelFormat`] layer
    /// on a *scoped* subscriber, returning the captured output. The runtime
    /// drives `step` inside the `with_default` scope, so the thread-local
    /// subscriber is what `render_error` resolves against.
    fn step_capturing_log(
        registry: &Registry,
        session: &mut Session,
        line: &str,
        ansi: bool,
    ) -> (ControlFlow<()>, String) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(crate::logfmt::CompactLevelFormat::new(ansi))
            .with_writer(BufMaker(Arc::clone(&buf)))
            .finish();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let flow = tracing::subscriber::with_default(subscriber, || {
            rt.block_on(step(registry, session, line))
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (flow, out)
    }

    /// [`step_capturing_log`]'s sibling for [`step_interruptible`]: same scoped
    /// subscriber and runtime, plus a `driver` future standing in for the SIGINT
    /// forwarder, since a test must never raise a real signal into a shared test
    /// binary. `dispatch` is bounded so a regression that drops or never wakes
    /// it fails an assertion instead of hanging the suite.
    fn capturing_log<F, D>(dispatch: F, driver: D) -> (StepOutcome, String)
    where
        F: Future<Output = StepOutcome>,
        D: Future<Output = ()>,
    {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(crate::logfmt::CompactLevelFormat::new(false))
            .with_writer(BufMaker(Arc::clone(&buf)))
            .finish();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let outcome = tracing::subscriber::with_default(subscriber, || {
            rt.block_on(async {
                let bounded = tokio::time::timeout(BOUND, dispatch);
                let (outcome, ()) = tokio::join!(bounded, driver);
                outcome.expect("the dispatch must settle within the bound")
            })
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (outcome, out)
    }

    /// [`capturing_log`]'s synchronous sibling, for the parts of the protocol
    /// that are pure decisions.
    fn capture_log<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(crate::logfmt::CompactLevelFormat::new(false))
            .with_writer(BufMaker(Arc::clone(&buf)))
            .finish();
        let value = tracing::subscriber::with_default(subscriber, f);
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (value, out)
    }

    /// One press, delivered once the probe reports it is parked mid-dispatch. A
    /// probe that never reports panics here rather than pressing into the void,
    /// which would be a green test that never exercised the interrupt.
    async fn press_once(started: oneshot::Receiver<()>, tx: mpsc::Sender<()>) {
        tokio::time::timeout(BOUND, started)
            .await
            .expect("the probe must report started")
            .expect("the probe's start signal must not be dropped");
        let _ = tx.send(()).await;
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

    #[test]
    fn eof_dispatches_quit_and_breaks() {
        // Targets the helper the Ctrl-D arm calls, not bare `step`, so this
        // stays pinned to the code that arm actually reaches.
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        let (outcome, _out) = capturing_log(
            step_interruptible(&r, &mut s, "EOF", &mut rx, OnPress::EscalateOnly),
            std::future::ready(()),
        );
        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert!(s.should_exit());
    }

    #[test]
    fn unknown_command_renders_error_and_continues() {
        let (r, runs) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (flow, out) = step_capturing_log(&r, &mut s, "nope", false);
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(out.contains("Unknown command"), "got: {out:?}");
        assert_eq!(out.lines().count(), 1, "rendered exactly once");
    }

    /// The single-channel error contract: exactly one `error: <message>` line,
    /// with no `tracing` target, timestamp or `err=` field.
    #[test]
    fn error_line_is_prefixed_and_free_of_tracing_noise() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_flow, out) = step_capturing_log(&r, &mut s, "nope", false);
        assert_eq!(out.lines().count(), 1, "rendered exactly once");
        assert!(
            out.starts_with("error: "),
            "must carry the `error: ` prefix, got: {out:?}"
        );
        assert!(!out.contains("mtui_cli"), "no tracing target");
        assert!(!out.contains("err="), "no structured field noise");
        assert!(!out.contains('Z'), "no ISO-8601 timestamp");
    }

    /// Under color, the `error` level token is red-wrapped while the message
    /// text is not — the same `CompactLevelFormat` layer that colors
    /// `info`/`warn`, so all three levels share one look.
    #[test]
    fn error_level_token_is_colorized_message_is_not() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_flow, out) = step_capturing_log(&r, &mut s, "nope", true);
        assert!(out.contains('\u{1b}'), "colorized, got: {out:?}");
        assert!(out.contains("error"), "level token present: {out:?}");
        assert!(out.contains("Unknown command"), "message present: {out:?}");
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

    /// The first Ctrl-C cancels the running command instead of killing the
    /// process: the body observes the seam, unwinds through its own `return`,
    /// and the loop keeps going.
    #[test]
    fn one_press_cancels_the_running_command_cooperatively() {
        let (started_tx, started_rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let mut r = Registry::new();
        r.register(Arc::new(ParkCmd {
            started: Mutex::new(Some(started_tx)),
            finished: Arc::clone(&finished),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "park", &mut rx, OnPress::Cancel),
            press_once(started_rx, tx),
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert!(
            finished.load(Ordering::SeqCst),
            "the parked body must be polled to completion, not dropped"
        );
        assert_eq!(
            out.matches("error: cancelled").count(),
            1,
            "exactly one cancelled line, got: {out:?}"
        );
        assert_eq!(
            out.matches("Ctrl-C again").count(),
            1,
            "exactly one notice, got: {out:?}"
        );
        assert_eq!(
            out.lines().count(),
            2,
            "the notice and the error, nothing else: {out:?}"
        );
        assert!(out.starts_with("warn: "), "notice comes first: {out:?}");
    }

    /// The token is one-shot, so a cancel must not outlive its own line: the
    /// next dispatch installs a fresh one and runs normally. Otherwise every
    /// later command — the `quit`/`EOF` teardown that releases the pool claims
    /// and host locks included — dies at the driver's pre-flight check.
    #[test]
    fn a_cancelled_command_does_not_poison_the_next_one() {
        let (started_tx, started_rx) = oneshot::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(ParkCmd {
            started: Mutex::new(Some(started_tx)),
            finished: Arc::clone(&finished),
        }));
        r.register(Arc::new(EchoCmd {
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let (cancelled, _) = capturing_log(
            step_interruptible(&r, &mut s, "park", &mut rx, OnPress::Cancel),
            press_once(started_rx, tx),
        );
        assert_eq!(cancelled, StepOutcome::Flow(ControlFlow::Continue(())));

        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "echo hi", &mut rx, OnPress::Cancel),
            std::future::ready(()),
        );
        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the next command ran");
        assert!(out.is_empty(), "and rendered nothing, got: {out:?}");
    }

    /// Presses from the cooked gap between commands (Ctrl-C while `edit` held
    /// the TTY) belong to no dispatch and must not cancel the *next* one. Two
    /// presses, not one: a drain removing only the first leaves the second to
    /// cancel the next command, or to consume its "already cancelling" slot so
    /// the next genuine press force-quits with no warning.
    #[test]
    fn a_press_from_the_idle_gap_does_not_cancel_the_next_command() {
        let (go_tx, go_rx) = oneshot::channel();
        let observed = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(GateCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::clone(&observed),
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        tx.try_send(()).expect("the queue has room");
        tx.try_send(()).expect("the queue has room for both");

        // Held at the gate for at least one poll, so an undrained press has
        // every chance to be observed.
        let driver = async move {
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "gate", &mut rx, OnPress::Cancel),
            driver,
        );

        assert_ne!(
            outcome,
            StepOutcome::Escalate,
            "stale presses must never force-quit"
        );
        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the command ran");
        assert!(
            !observed.load(Ordering::SeqCst),
            "a stale press must not reach this command's token"
        );
        assert!(
            out.is_empty(),
            "no notice and no error on the normal path, got: {out:?}"
        );
    }

    /// A press landing in the same poll window as the command's own completion
    /// belongs to the *next* dispatch. Without the `biased;` this is a coin
    /// flip, and losing it force-quits a command that finished cleanly: it warns
    /// about released locks and skips the `quit` teardown, genuinely stranding
    /// the pool claims the warning only speculated about.
    #[test]
    fn a_press_racing_completion_never_force_quits() {
        for round in 0..RACE_ROUNDS {
            let (go_tx, go_rx) = oneshot::channel();
            let runs = Arc::new(AtomicUsize::new(0));
            let mut r = Registry::new();
            r.register(Arc::new(GateCmd {
                go: Mutex::new(Some(go_rx)),
                observed_cancel: Arc::new(AtomicBool::new(false)),
                runs: Arc::clone(&runs),
            }));
            let (mut s, _buf) = session_with_buffer();
            let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

            // Gate release and both presses land before the loop is polled
            // again: the race, made deterministic.
            let driver = async move {
                tokio::task::yield_now().await;
                let _ = go_tx.send(());
                tx.try_send(()).expect("the queue has room");
                tx.try_send(()).expect("the queue has room for both");
            };
            let (outcome, out) = capturing_log(
                step_interruptible(&r, &mut s, "gate", &mut rx, OnPress::Cancel),
                driver,
            );

            assert_eq!(
                outcome,
                StepOutcome::Flow(ControlFlow::Continue(())),
                "round {round}: the completed command must win the race"
            );
            assert_eq!(runs.load(Ordering::SeqCst), 1, "round {round}");
            assert!(
                out.is_empty(),
                "round {round}: no notice for a command that had already finished, got: {out:?}"
            );
            assert_eq!(
                rx.len(),
                2,
                "round {round}: both presses stay queued for the next dispatch"
            );
        }
    }

    /// A second press escalates: some bodies never observe the seam (`run`,
    /// `install`), so the operator keeps an escape hatch. The decision is
    /// returned here; [`Repl::run`] executes the process exit.
    #[test]
    fn a_second_press_escalates_to_a_forced_exit() {
        let (started_tx, started_rx) = oneshot::channel();
        let mut r = Registry::new();
        r.register(Arc::new(DeafCmd {
            started: Mutex::new(Some(started_tx)),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let driver = async move {
            tokio::time::timeout(BOUND, started_rx)
                .await
                .expect("the probe must report started")
                .expect("the probe's start signal must not be dropped");
            // The queue holds both, fixing the consumption order however the
            // tasks interleave.
            let _ = tx.send(()).await;
            let _ = tx.send(()).await;
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "deaf", &mut rx, OnPress::Cancel),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Escalate);
        assert_eq!(
            out.matches("Ctrl-C again").count(),
            1,
            "the first press explained itself exactly once: {out:?}"
        );
    }

    /// A dead forwarder must not take the dispatch down with it: the closed
    /// channel yields `None` forever and the loop has to see the command through
    /// rather than abandon it or spin. The probe parks so the branch is actually
    /// reached — a command finishing on its first poll would pass regardless.
    #[test]
    fn a_closed_interrupt_channel_still_completes_the_dispatch() {
        let (go_tx, go_rx) = oneshot::channel();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(GateCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::new(AtomicBool::new(false)),
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        drop(tx);

        let driver = async move {
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "gate", &mut rx, OnPress::Cancel),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the command still ran");
        assert!(out.is_empty(), "and rendered nothing, got: {out:?}");
    }

    /// Ctrl-D after a cancelled command must still reach `quit`: the sharpest
    /// edge of the one-shot token, where the earlier cancel would bail the
    /// teardown out at the driver's pre-flight check. `commands/quit.rs` pins
    /// what `quit` then does; this pins that it is entered at all.
    #[test]
    fn ctrl_d_tears_down_even_after_a_cancelled_command() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        // The state a cancelled line leaves behind.
        s.cancel_token().cancel();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let (outcome, _out) = capturing_log(
            step_interruptible(&r, &mut s, "EOF", &mut rx, OnPress::EscalateOnly),
            std::future::ready(()),
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert!(s.should_exit(), "the teardown ran and asked to exit");
    }

    /// A press during the teardown must **not** cancel it — that is what strands
    /// the locks. The teardown runs to completion; the press buys a warning.
    #[test]
    fn a_press_during_the_teardown_does_not_cancel_it() {
        let (go_tx, go_rx) = oneshot::channel();
        let observed = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(SlowQuitCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::clone(&observed),
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        // One press while the teardown is parked, then let it finish.
        let driver = async move {
            tokio::task::yield_now().await;
            let _ = tx.send(()).await;
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "EOF", &mut rx, OnPress::EscalateOnly),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the teardown finished");
        assert!(
            !observed.load(Ordering::SeqCst),
            "a press must never cancel the teardown's token"
        );
        assert_eq!(
            out.matches("teardown in progress").count(),
            1,
            "warned exactly once, got: {out:?}"
        );
    }

    /// The teardown can genuinely hang — `quit`'s pool-claim release has no
    /// timeout, so a blackholed refhost parks it indefinitely. With the handler
    /// armed, a second press must offer the way out that a fatal Ctrl-C used to,
    /// or Ctrl-C does nothing at all on this path.
    #[test]
    fn two_presses_during_the_teardown_force_quit() {
        // No `go` sender is ever fired: this teardown never finishes.
        let (_go_tx, go_rx) = oneshot::channel();
        let mut r = Registry::new();
        r.register(Arc::new(SlowQuitCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::new(AtomicBool::new(false)),
            runs: Arc::new(AtomicUsize::new(0)),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let driver = async move {
            tokio::task::yield_now().await;
            let _ = tx.send(()).await;
            let _ = tx.send(()).await;
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "EOF", &mut rx, OnPress::EscalateOnly),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Escalate);
        assert_eq!(
            out.matches("teardown in progress").count(),
            1,
            "the first press explained itself exactly once, got: {out:?}"
        );
    }

    /// A press that raced the *previous* dispatch's completion belongs to that
    /// dispatch, so the teardown must drain it; otherwise a single genuine press
    /// during a hung teardown force-quits with no warning.
    #[test]
    fn the_teardown_drains_a_press_left_over_from_the_last_dispatch() {
        let (go_tx, go_rx) = oneshot::channel();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(SlowQuitCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::new(AtomicBool::new(false)),
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        tx.try_send(()).expect("the queue has room");
        tx.try_send(()).expect("the queue has room for both");

        let driver = async move {
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "EOF", &mut rx, OnPress::EscalateOnly),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the teardown finished");
        assert!(out.is_empty(), "stale presses warn about nothing: {out:?}");
    }

    /// `Normal` returns from `main` so every destructor runs, reedline's history
    /// flush included; `ForceQuit` carries 128 + `SIGINT`, the status a Ctrl-C
    /// death would have had.
    #[test]
    fn the_force_quit_exit_status_is_128_plus_sigint() {
        assert_eq!(ReplExit::Normal.status(), None);
        assert_eq!(
            ReplExit::ForceQuit.status(),
            Some(mtui_core::ExitStatus::Interrupted)
        );
        assert_eq!(i32::from(mtui_core::ExitStatus::Interrupted), 130);
    }

    /// Only a bare `quit`/`exit`/`EOF` in **command position** is the teardown.
    /// Registry resolution routes aliases without enumerating them, and stops
    /// `help quit` from silently getting teardown semantics.
    #[test]
    fn only_a_quit_line_takes_the_teardown_path() {
        let (r, _) = registry();
        for line in ["quit", "exit", "EOF", "  quit  ", "quit reboot"] {
            assert_eq!(
                press_policy(&r, line),
                OnPress::EscalateOnly,
                "{line:?} dispatches the teardown"
            );
        }
        for line in [
            "echo hi",
            // `quit` is an argument here, not the command.
            "echo quit",
            // Neither resolves to a command at all: no command, no teardown.
            "help quit",
            "quitx",
            "",
            // Unbalanced quotes: the engine will render the syntax error.
            "echo \"unbalanced",
        ] {
            assert_eq!(
                press_policy(&r, line),
                OnPress::Cancel,
                "{line:?} is an ordinary line"
            );
        }
    }

    /// A **typed** `quit` dispatches the very teardown Ctrl-D does, so a press
    /// must not cancel it there either: unrouted, the first press cancels the
    /// cleanup's token and the second's advice omits the stranded pool claim.
    #[test]
    fn a_typed_quit_is_never_cancelled_by_a_press() {
        let (go_tx, go_rx) = oneshot::channel();
        let observed = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut r = Registry::new();
        r.register(Arc::new(SlowQuitCmd {
            go: Mutex::new(Some(go_rx)),
            observed_cancel: Arc::clone(&observed),
            runs: Arc::clone(&runs),
        }));
        let (mut s, _buf) = session_with_buffer();
        let (tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        // Exactly the decision `Repl::run` makes for this line.
        let on_press = press_policy(&r, "quit");

        let driver = async move {
            tokio::task::yield_now().await;
            let _ = tx.send(()).await;
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "quit", &mut rx, on_press),
            driver,
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the teardown finished");
        assert!(
            !observed.load(Ordering::SeqCst),
            "a typed quit's teardown must not be cancelled by a press"
        );
        assert_eq!(
            out.matches("teardown in progress").count(),
            1,
            "and it gets the teardown's notice, not a command's: {out:?}"
        );
    }

    /// Each force-quit record names the remedy for what *that* arm abandons — a
    /// command its operation locks, a teardown the pool claims too — and both
    /// must survive `set_log_level error`, hence `error!` not `warn!`.
    #[test]
    fn each_escalation_names_the_remedy_for_what_it_abandons() {
        let (exit, out) = capture_log(|| on_escalate(OnPress::Cancel));
        assert_eq!(exit, ReplExit::ForceQuit);
        assert!(
            out.starts_with("error: "),
            "must outrank `set_log_level error`, got: {out:?}"
        );
        assert!(out.contains("mid-command"), "{out:?}");
        assert!(out.contains("unlock --force"), "{out:?}");
        assert!(
            !out.contains("unlock --pool"),
            "a mid-command exit strands no pool claim: {out:?}"
        );

        let (exit, out) = capture_log(|| on_escalate(OnPress::EscalateOnly));
        assert_eq!(exit, ReplExit::ForceQuit);
        assert!(
            out.starts_with("error: "),
            "must outrank `set_log_level error`, got: {out:?}"
        );
        assert!(out.contains("mid-teardown"), "{out:?}");
        assert!(
            out.contains("unlock --force") && out.contains("unlock --pool"),
            "a teardown abandons both kinds of lock: {out:?}"
        );
    }

    /// `quit`'s `Break` survives the interruptible wrapper — the loop must still
    /// exit on it, not fall through to another prompt.
    #[test]
    fn quit_breaks_through_the_interruptible_path() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        let (outcome, _out) = capturing_log(
            step_interruptible(&r, &mut s, "quit", &mut rx, OnPress::Cancel),
            std::future::ready(()),
        );
        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert!(s.should_exit());
    }

    #[test]
    fn bad_flag_renders_error_and_continues() {
        let (r, runs) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (flow, out) = step_capturing_log(&r, &mut s, "echo --no-such-flag", false);
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the body never ran");
        assert!(!out.is_empty(), "usage error is rendered");
        // clap's own "error: " prefix survives once, not doubled with mtui's.
        assert_eq!(
            out.matches("error: ").count(),
            1,
            "exactly one error prefix, got: {out:?}"
        );
        assert!(!out.contains("error: error:"), "no doubled prefix: {out:?}");
    }
}
