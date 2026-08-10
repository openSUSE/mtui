//! The interactive REPL: read → dispatch → repeat.
//!
//! Every line is handed to [`mtui_core::dispatch_line`] (the *same* engine the
//! MCP surface dispatches through), whose typed [`EngineError`] the loop renders
//! and then keeps going — a bad command never tears down the session.
//!
//! Control keys map onto [`reedline::Signal`]:
//!
//! * `Signal::Success(line)` → dispatch the line (empty lines are a no-op in the
//!   engine), render any error, then honour a pending `quit`
//!   ([`Session::should_exit`]).
//! * `Signal::CtrlC` → abort the current input and reprompt (Ctrl-C on
//!   a partial line clears it); never exits.
//! * `Signal::CtrlD` → graceful session exit (Ctrl-D → `EOF` alias of
//!   `quit`): break the loop, process exit 0.
//!
//! Ctrl-C means two different things because the terminal is in two different
//! modes. *While reading a line* reedline holds raw mode, so Ctrl-C is the key
//! event above. *While a command runs* the terminal is cooked and Ctrl-C is a
//! real SIGINT — which used to kill the process outright, skipping every
//! teardown and stranding a dead-pid operation lock on each locked host. It is
//! now forwarded onto the session's cancellation seam instead
//! (`spawn_interrupt_forwarder` → `step_interruptible`): the first press
//! cancels the running command at its next checkpoint, a second press
//! force-quits with a warning about the locks that may be left behind.
//! During the Ctrl-D teardown the same presses escalate but never *cancel* —
//! `step_teardown` explains why.
//!
//! The read loop and dispatch are independent of the editor's input features:
//! tab completion, persistent history + Ctrl-R reverse-search + inline
//! hint, and the workflow-aware prompt + RRID status + input highlighter
//! all live in the [`Reedline`] builder / [`MtuiPrompt`] in [`Repl::new`];
//! the command-timeout prompter is wired in at the composition root
//! (`main.rs`) without changing this loop.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use mtui_core::{EngineError, Registry, Session, dispatch_line};
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

/// How many unread Ctrl-C presses the forwarder queues before coalescing.
///
/// Two is the whole protocol: one press to cancel, one to force-quit. A third
/// while both still sit unread says nothing the second did not, so dropping it
/// is correct — the same coalescing the kernel and `tokio`'s own signal driver
/// already do.
const INTERRUPT_QUEUE: usize = 2;

/// The exit status of a force-quit: 128 + `SIGINT`, the shell convention for a
/// process killed by Ctrl-C (which is what would have happened before this
/// loop started handling the signal).
const EXIT_INTERRUPTED: i32 = 130;

/// How [`Repl::run`] ended.
///
/// A force-quit is *decided* in the loop and *executed* by the caller, because
/// the process must not exit until the editor has been dropped: reedline
/// persists its `FileBackedHistory` on drop, and `std::process::exit` runs no
/// destructors. Exiting from inside the loop would silently discard the
/// session's command history — a second cost on top of the stranded locks, and
/// one the operator never asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplExit {
    /// `quit`/Ctrl-D: the teardown ran, the process exits normally.
    Normal,
    /// A double Ctrl-C: the caller drops the REPL (flushing history) and exits
    /// with [`ReplExit::exit_code`].
    ForceQuit,
}

impl ReplExit {
    /// The status to `exit` with, or `None` to return from `main` normally.
    #[must_use]
    pub fn exit_code(self) -> Option<i32> {
        match self {
            Self::Normal => None,
            Self::ForceQuit => Some(EXIT_INTERRUPTED),
        }
    }
}

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
    /// `file_backed_history` persisting to
    /// `$XDG_DATA_HOME/mtui/history` (with Ctrl-R reverse-search from the default
    /// emacs bindings); and a [`DefaultHinter`] showing the greyed inline
    /// suggestion. The dynamic prompt/toolbar comes from [`MtuiPrompt`]; the
    /// command-timeout prompter is wired in separately at the composition
    /// root (`main.rs`).
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
    /// Returns how the session ended: [`ReplExit::Normal`], or
    /// [`ReplExit::ForceQuit`] when a double Ctrl-C asked to stop waiting for a
    /// command (or a teardown) to finish. The caller executes that decision —
    /// see [`ReplExit`] for why the exit cannot happen here.
    ///
    /// # Errors
    ///
    /// Propagates a fatal editor I/O error from [`Reedline::read_line`] (e.g. a
    /// broken terminal). Command failures are *not* errors here: they are
    /// rendered to the session display and the loop continues.
    ///
    /// The session guard is held across the dispatch's `.await`
    /// (`clippy::await_holding_lock`, allowed below). It is sound because
    /// nothing else can want the lock at that point: the editor's synchronous
    /// `read_line` (the only other lock holder, via the completer and the
    /// highlighter) has already returned before we lock, and no host tasks are
    /// in flight mid-line. The runtime is multi-threaded, so a *contending*
    /// task would genuinely block a worker here — there simply is none, and
    /// the interrupt forwarder spawned below deliberately touches no session
    /// state (it only moves a unit through a channel). A
    /// `tokio::sync::Mutex` is the usual remedy, but its `blocking_lock` panics
    /// inside `read_line`'s runtime context and its async `lock` is unreachable
    /// from the synchronous completer, so the std `Mutex` + a scoped allow is the
    /// correct fit for this strictly-alternating editor↔dispatch bridge.
    #[allow(clippy::await_holding_lock)]
    pub async fn run(&mut self) -> anyhow::Result<ReplExit> {
        {
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            session.display.println(INTRO);
        }
        // Arm SIGINT before the first prompt, for the whole session (see the
        // forwarder's own note on why arming early is what makes Ctrl-C
        // deterministic here).
        let mut interrupts = spawn_interrupt_forwarder();

        loop {
            match self.line_editor.read_line(&self.prompt)? {
                Signal::Success(line) => {
                    // `shell` attaches an interactive PTY, which needs the local
                    // TTY this REPL owns — the engine (shared with headless MCP)
                    // can't do that, so its `shell` command is a headless-error
                    // stub. Intercept the line *before* dispatch and drive the
                    // raw-mode bridge here instead. Any other line
                    // takes the normal engine path. A shell line never exits.
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
                    // `edit` spawns `$EDITOR` on the controlling TTY (the same
                    // reason as `shell`): the shared engine has no terminal, so
                    // its `edit` command is a headless-error stub. Intercept the
                    // line here and spawn the (blocking, foregrounded) editor,
                    // which owns the TTY for its lifetime; report any failure
                    // through `tracing::error!` like every other error. An edit
                    // line never exits.
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
                    // Lock only for the dispatch; the completer's own lock during
                    // `read_line` was released before this returned. (Guard held
                    // across the await — justified on `run`'s doc comment.)
                    let outcome = {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        step_interruptible(&self.registry, &mut session, &line, &mut interrupts)
                            .await
                    };
                    match outcome {
                        StepOutcome::Flow(flow) => {
                            if flow.is_break() {
                                break;
                            }
                        }
                        // A second Ctrl-C: the operator has decided not to wait
                        // for the cooperative stop. Honour it — loudly, because
                        // this is the one path that *can* leave the locks a
                        // clean stop would have released.
                        StepOutcome::Escalate => {
                            tracing::warn!(
                                "forcing exit mid-command; operation locks may remain on the \
                                 update's hosts — release them with `unlock --force` from a new \
                                 session"
                            );
                            return Ok(ReplExit::ForceQuit);
                        }
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
                // Ctrl-D: graceful session exit via the `EOF` alias of `Quit`.
                // Dispatch the `EOF` command through the engine so the full quit
                // teardown runs (pool-claim release + host close), then break —
                // a bare `break` would skip that cleanup. reedline persists the
                // `FileBackedHistory` when the editor is dropped after `run`
                // returns, so no explicit history flush is needed here.
                Signal::CtrlD => {
                    let outcome = {
                        let mut session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        step_teardown(&self.registry, &mut session, &mut interrupts).await
                    };
                    // A double Ctrl-C during the teardown: the same escape
                    // hatch as mid-command, and the same honest warning. What
                    // the teardown had not reached yet stays claimed/locked.
                    if outcome == StepOutcome::Escalate {
                        tracing::warn!(
                            "forcing exit mid-teardown; pool claims and operation locks may \
                             remain on the update's hosts — release them with `unlock --force` \
                             and `unlock --pool` from a new session"
                        );
                        return Ok(ReplExit::ForceQuit);
                    }
                    break;
                }
                // `#[non_exhaustive]`: any future/host signal is ignored and we
                // simply reprompt rather than crashing the session.
                _ => {}
            }
        }

        Ok(ReplExit::Normal)
    }
}

/// Spawns the SIGINT forwarder and hands back the channel the dispatch loop
/// reads presses from.
///
/// The task is spawned once per [`Repl::run`] and outlives every dispatch:
/// `tokio::signal::ctrl_c` installs its handler *process-wide* on first use and
/// never uninstalls it, so arming here — before the first prompt — is what
/// makes Ctrl-C mean one thing for the whole session. (It previously depended
/// on history: `request_review --watch` armed the handler as a side effect, so
/// after one watch a later Ctrl-C was silently swallowed instead of killing
/// the process.)
///
/// While `read_line` owns the terminal reedline holds raw mode, so Ctrl-C is a
/// key event and no signal is raised at all; the forwarder therefore only ever
/// sees presses from a cooked window — a running command, or a gap between
/// commands (which [`step_interruptible`] drains).
///
/// "The whole session" means *from the first prompt onward*. Startup seeding
/// (`-a`/`-k`/`--sut`: an SVN checkout, refhost connects, pool claims) runs
/// before this, and a Ctrl-C there still kills the process the old way. Arming
/// earlier without a consumer would be worse, not better — it would make Ctrl-C
/// during a 60-second connect a silent no-op — so routing the seeding through
/// this same protocol is the fix, not moving this call.
///
/// Only the wiring lives here. The effect it drives is [`step_interruptible`],
/// which is tested by injecting on this same channel: raising a real `SIGINT`
/// inside a shared test binary would kill or cross-contaminate every other
/// test in it, so the signal source itself stays deliberately untested.
fn spawn_interrupt_forwarder() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(INTERRUPT_QUEUE);
    tokio::spawn(async move {
        // A full queue already holds an unread press (coalesce); a closed one
        // means the loop is gone and nothing will read again.
        let forward = |tx: &mpsc::Sender<()>| {
            !matches!(tx.try_send(()), Err(mpsc::error::TrySendError::Closed(())))
        };
        // One long-lived stream, created *before* the first press. Calling
        // `ctrl_c()` per press mints a fresh subscription each time, and a
        // subscription only sees signals arriving after its first poll — so a
        // second press landing between two calls would be lost. Narrow, but
        // free to close, and a persistent stream is tokio's own shape for
        // handling a signal repeatedly.
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
                // The handler could not be installed — SIGINT keeps its default
                // disposition (it kills, as before this loop existed) and
                // retrying would only spin.
                Err(e) => {
                    tracing::debug!(error = %e, "no SIGINT handler; Ctrl-C stays fatal");
                }
            }
        }
        // No `SignalKind` off unix; `ctrl_c` is the portable equivalent, at the
        // cost of the re-subscribe window described above.
        #[cfg(not(unix))]
        while tokio::signal::ctrl_c().await.is_ok() {
            if !forward(&tx) {
                break;
            }
        }
    });
    rx
}

/// What one interruptible dispatch decided.
///
/// Split from the execution so the decision is testable: [`Repl::run`] turns
/// [`Escalate`](Self::Escalate) into the process exit, which a test cannot
/// observe without taking the whole test binary with it.
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
/// The interruptible sibling of [`step`], and like it deliberately
/// TTY-free: presses arrive on `interrupts` (the
/// [`spawn_interrupt_forwarder`] channel in the live REPL, a test's own sender
/// otherwise), so the whole protocol is exercisable without raising a signal.
///
/// Three constraints shape the body:
///
/// * **A fresh token per dispatch, installed unconditionally.** A
///   [`CancellationToken`] is one-shot and this loop never resets it, so a
///   cancel would otherwise poison *every* later dispatch — including the
///   `quit`/`EOF` teardown, which would die at the `Command::run` pre-flight
///   check and strand exactly the pool claims and host locks a cooperative
///   cancel exists to release. Installing unconditionally (rather than only
///   after a cancel) is the MCP job layer's self-healing shape.
/// * **Stale presses are drained first.** A Ctrl-C from a cooked gap between
///   commands belongs to no dispatch and must not cancel the next one.
/// * **The dispatch future is never dropped on a press.** Cancelling the token
///   asks the flow to stop at its next checkpoint; dropping the future instead
///   would abandon it mid-step, which is the very teardown hole this replaces.
///   Escalation is the one exception, and it is the operator's explicit second
///   request.
///
/// What the first press actually buys depends on the command: checkpointed
/// flows (`update`, `prepare`, `downgrade`, every fan-out boundary) stop
/// promptly with their locks released, while a host operation already under
/// way (`run`, `install`, `uninstall`) finishes first and unlocks normally.
async fn step_interruptible(
    registry: &Registry,
    session: &mut Session,
    line: &str,
    interrupts: &mut mpsc::Receiver<()>,
) -> StepOutcome {
    // Stale presses, then a token this dispatch owns (both per the contract
    // above).
    while interrupts.try_recv().is_ok() {}

    session.set_cancel_token(CancellationToken::new());
    // Clone before `step` borrows the session mutably; clones share state, so
    // cancelling this one cancels the session's.
    let token = session.cancel_token();

    let dispatch = step(registry, session, line);
    tokio::pin!(dispatch);
    let mut cancelling = false;
    loop {
        let press = tokio::select! {
            // Biased, completion first: a press landing in the same poll window
            // as the command's own completion must not be attributed to it.
            // Unbiased, that race would force-quit a command that had just
            // finished cleanly — warning about locks it had already released,
            // and skipping the `quit` teardown that would have released the
            // pool claims. A press that loses this race stays queued and is
            // drained by the next dispatch, where it belongs.
            biased;
            flow = &mut dispatch => return StepOutcome::Flow(flow),
            press = interrupts.recv() => press,
        };
        if press.is_none() {
            // The forwarder is gone (it only exits when SIGINT cannot be
            // handled at all), so nothing can interrupt this dispatch: see it
            // through instead of spinning on a closed channel.
            return StepOutcome::Flow((&mut dispatch).await);
        }
        if cancelling {
            return StepOutcome::Escalate;
        }
        cancelling = true;
        token.cancel();
        // Emitted only on the signal path, so the normal dispatch's
        // exactly-one-`error: `-line contract is untouched.
        tracing::warn!(
            "cancelling — the command stops at its next checkpoint (a host operation already \
             under way finishes first); press Ctrl-C again to force-quit, which may strand \
             operation locks"
        );
    }
}

/// Dispatches the `EOF` alias of `quit` for Ctrl-D, on a token that cannot
/// already be cancelled, with an **escalate-only** interrupt arm.
///
/// Three deliberate differences from [`step_interruptible`]:
///
/// * It installs a fresh token for the same reason — the previous line may have
///   been cancelled, and this dispatch is precisely the one that must not die at
///   the driver's pre-flight check, since it releases the pool claims and closes
///   the hosts.
/// * A press here **never cancels the token.** Cancelling the cleanup is what
///   strands the locks; the teardown always runs to completion on its own.
/// * A press still *counts*, though, because the teardown can genuinely hang:
///   `quit`'s pool-claim release has no timeout of its own, so a blackholed
///   refhost parks it indefinitely. Before this loop existed Ctrl-C killed the
///   process outright there; with the handler armed it would do nothing at all
///   unless this path offers the same escape. So the first press warns and the
///   second returns [`StepOutcome::Escalate`].
async fn step_teardown(
    registry: &Registry,
    session: &mut Session,
    interrupts: &mut mpsc::Receiver<()>,
) -> StepOutcome {
    // Presses that raced the *previous* dispatch's completion belong to it, not
    // to this teardown (see the biased select above).
    while interrupts.try_recv().is_ok() {}

    session.set_cancel_token(CancellationToken::new());

    let teardown = step(registry, session, "EOF");
    tokio::pin!(teardown);
    let mut warned = false;
    loop {
        let press = tokio::select! {
            biased;
            flow = &mut teardown => return StepOutcome::Flow(flow),
            press = interrupts.recv() => press,
        };
        if press.is_none() {
            return StepOutcome::Flow((&mut teardown).await);
        }
        if warned {
            return StepOutcome::Escalate;
        }
        warned = true;
        tracing::warn!(
            "teardown in progress (releasing pool claims, closing hosts) — it cannot be \
             cancelled; press Ctrl-C again to force-quit, which may leave pool claims and \
             operation locks behind"
        );
    }
}

/// Dispatches one input `line` and reports whether the loop should stop.
///
/// This is the testable heart of the loop, deliberately independent of the
/// TTY-bound [`Reedline`] editor: it dispatches through the shared engine,
/// reports any error through `tracing::error!` exactly once (the single
/// operator log channel, alongside `info`/`warn`), and reports
/// [`ControlFlow::Break`] iff the `quit` command asked the session to exit.
///
/// An empty/whitespace line is a no-op ([`ControlFlow::Continue`]) — the engine
/// already treats it as such.
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
/// Errors, warnings, and info all flow through the *same* path here: the
/// `tracing` subscriber installed by [`init_tracing`](crate::init_tracing), whose
/// [`CompactLevelFormat`](crate::logfmt::CompactLevelFormat) renders every level
/// as a lowercased, colorized token — `error: <message>` (red), `warn: …`
/// (yellow), `info: …` (green) — with one shared `--color` decision. The error
/// message is the event's *message* (not a structured field), so no `err=`
/// noise appears; the default format carries no timestamp/target either.
///
/// This deliberately differs from the headless `mtui-mcp` surface, which has
/// no `tracing` subscriber and renders the same `error: <message>` text
/// through its captured display buffer. The two present failures with
/// identical *text*; each uses the channel appropriate to its surface
/// (REPL → operator log on stderr; headless → captured display sink).
///
/// One exception: a genuine usage error (`Parse { help_or_version: false,
/// .. }`) carries clap's *own* already-rendered `error: ` prefix (and
/// whatever color clap itself applied). Emitting it through the normal path
/// would double that prefix, so it's tagged with
/// [`CLAP_PREFIXED_TARGET`](crate::logfmt::CLAP_PREFIXED_TARGET) instead,
/// which `CompactLevelFormat` recognizes and renders without adding a second
/// one. `--help`/`--version` output (`help_or_version: true`) keeps flowing
/// through the normal path unchanged.
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

    /// A generous upper bound on anything the interrupt tests wait for. The
    /// work is in-process channel traffic, so overrunning it means a hang, not
    /// a slow machine.
    ///
    /// What it buys: a dispatch that is *parked* — dropped, never woken, or
    /// waiting on a cancel that never came — fails an assertion instead of
    /// wedging the test binary. What it does not buy: a busy-spinning
    /// regression still hangs, since nothing here can preempt one.
    const BOUND: Duration = Duration::from_secs(5);

    /// How many independent races [`a_press_racing_completion_never_force_quits`]
    /// runs. The `biased;` select must win every one; an unbiased select picks
    /// the interrupt branch about half the time, so it survives all 32 rounds
    /// with probability 2⁻³² — a statistical proof, but not a meaningfully
    /// uncertain one.
    const RACE_ROUNDS: usize = 32;

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

    /// A command that parks on the cancellation seam and — crucially — records
    /// that it was polled all the way to its own `return` afterwards. That
    /// flag is the observable proof the dispatch future was *not* dropped when
    /// the interrupt arrived, which is the whole difference between a
    /// cooperative cancel and the process kill this replaces.
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

    /// A command that never observes the seam — the `run`/`install` shape,
    /// where a cancel is inert until the host operation finishes. Only a
    /// second press can get the operator out of one.
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
    /// cancelled behind its back. It parks first so a stale press cannot be
    /// missed by a dispatch that finished before the loop ever looked at the
    /// interrupt channel.
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

    /// A `quit` that takes its time — the shape of the real one when a refhost
    /// blackholes: `quit`'s pool-claim release has no timeout, so the teardown
    /// parks. Records whether anything cancelled its token, which nothing on the
    /// teardown path is allowed to do.
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

    /// Runs `step` on `line` with the REPL's real [`CompactLevelFormat`] layer
    /// installed on a *scoped* subscriber, returning the captured log output.
    /// This exercises the exact operator-facing error path the REPL uses.
    ///
    /// A local current-thread runtime drives the async `step` inside the
    /// `with_default` scope, so the scoped subscriber (thread-local) is the one
    /// `render_error`'s `tracing::error!` resolves against.
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

    /// [`step_capturing_log`]'s sibling for the interrupt-aware dispatches
    /// ([`step_interruptible`] and [`step_teardown`]): same scoped subscriber
    /// and same current-thread runtime, plus a `driver` future standing in for
    /// the SIGINT forwarder (the real one is thin wiring — a test must never
    /// raise a real signal into a shared test binary).
    ///
    /// `dispatch` is bounded so a regression that drops or never wakes it fails
    /// an assertion instead of hanging the suite.
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

    /// One press, delivered once the probe reports it is parked mid-dispatch.
    /// A probe that never reports panics here rather than quietly pressing into
    /// the void — a missed handshake would otherwise turn into a green test that
    /// never exercised the interrupt at all.
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
        // What the Ctrl-D arm calls: `step_teardown` dispatches the `EOF` alias
        // through the engine (so the full quit teardown runs), which must break
        // the loop and set exit. Targeting the helper rather than bare `step`
        // keeps this pinned to the code the arm actually reaches.
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        let (outcome, _out) =
            capturing_log(step_teardown(&r, &mut s, &mut rx), std::future::ready(()));
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

    /// The de-noised, single-channel error contract: a
    /// failing command yields exactly one `error: <message>` line through the
    /// operator log, with none of the old `tracing` noise (a target, a
    /// timestamp, or an `err=` field).
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
    /// text is not colorized — the same single `CompactLevelFormat` layer that
    /// colors `info`/`warn`, so all three levels share one look.
    #[test]
    fn error_level_token_is_colorized_message_is_not() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_flow, out) = step_capturing_log(&r, &mut s, "nope", true);
        // The red escape wraps the lowercased `error` token before `: `.
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
    /// and the loop keeps going. Rendering stays exactly one `error: cancelled`
    /// line, with the notice on the signal path beside it.
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
            step_interruptible(&r, &mut s, "park", &mut rx),
            press_once(started_rx, tx),
        );

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        // The body reached its own `return` *after* the cancel: the dispatch
        // future was cancelled cooperatively, never dropped mid-step.
        assert!(
            finished.load(Ordering::SeqCst),
            "the parked body must be polled to completion, not dropped"
        );
        assert_eq!(
            out.matches("error: cancelled").count(),
            1,
            "exactly one cancelled line, got: {out:?}"
        );
        // The notice fires once, and only from the signal path.
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
    /// next dispatch installs a fresh one and runs normally. Without that, every
    /// later command — including the `quit`/`EOF` teardown that releases the
    /// pool claims and host locks — would die at the driver's pre-flight check.
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
            step_interruptible(&r, &mut s, "park", &mut rx),
            press_once(started_rx, tx),
        );
        assert_eq!(cancelled, StepOutcome::Flow(ControlFlow::Continue(())));

        let (outcome, out) = capturing_log(
            step_interruptible(&r, &mut s, "echo hi", &mut rx),
            std::future::ready(()),
        );
        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the next command ran");
        assert!(out.is_empty(), "and rendered nothing, got: {out:?}");
    }

    /// Presses that arrive while no command is running (the cooked gap between
    /// commands — e.g. Ctrl-C while `edit` held the TTY) belong to no dispatch
    /// and must not cancel the *next* one.
    ///
    /// Two presses, not one: an operator who gets no response from an editor
    /// presses again, and a drain that removed only the first would leave the
    /// second to cancel the next command — or, worse, to consume its "already
    /// cancelling" slot so that the *next* genuine press force-quits with no
    /// warning at all.
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

        // Hold the command at its gate for at least one poll, so a press that
        // was *not* drained has every chance to be observed.
        let driver = async move {
            tokio::task::yield_now().await;
            let _ = go_tx.send(());
        };
        let (outcome, out) = capturing_log(step_interruptible(&r, &mut s, "gate", &mut rx), driver);

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
    /// belongs to the *next* dispatch, not this one.
    ///
    /// Without the `biased;` (completion first) this is a coin flip, and losing
    /// it is expensive: a command that finished cleanly would force-quit the
    /// session, warn about locks it had already released, and skip the `quit`
    /// teardown — genuinely stranding the pool claims the warning only
    /// speculated about. Both presses must instead stay queued for the next
    /// dispatch to drain.
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

            // Release the gate *and* queue both presses before the loop is
            // polled again, so the completion and the presses are ready in the
            // same poll window — the race, made deterministic.
            let driver = async move {
                tokio::task::yield_now().await;
                let _ = go_tx.send(());
                tx.try_send(()).expect("the queue has room");
                tx.try_send(()).expect("the queue has room for both");
            };
            let (outcome, out) =
                capturing_log(step_interruptible(&r, &mut s, "gate", &mut rx), driver);

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
    /// `install` — a host operation already under way finishes first), so the
    /// operator keeps an escape hatch. The decision is returned here;
    /// [`Repl::run`] is what executes the process exit.
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
            // The queue holds both, so the order of consumption is fixed
            // however the tasks interleave.
            let _ = tx.send(()).await;
            let _ = tx.send(()).await;
        };
        let (outcome, out) = capturing_log(step_interruptible(&r, &mut s, "deaf", &mut rx), driver);

        assert_eq!(outcome, StepOutcome::Escalate);
        assert_eq!(
            out.matches("Ctrl-C again").count(),
            1,
            "the first press explained itself exactly once: {out:?}"
        );
    }

    /// A dead forwarder must not take the dispatch down with it: the closed
    /// channel yields `None` forever, and the loop has to see the command
    /// through rather than abandon it (or spin).
    ///
    /// The probe parks, so the `None` is genuinely what the loop reacts to — a
    /// command that finished on its first poll would let this pass without ever
    /// reaching the branch.
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
        let (outcome, out) = capturing_log(step_interruptible(&r, &mut s, "gate", &mut rx), driver);

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Continue(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the command still ran");
        assert!(out.is_empty(), "and rendered nothing, got: {out:?}");
    }

    /// Ctrl-D after a cancelled command must still reach the `quit` command.
    /// This is the sharpest edge of the one-shot token: the cancel that stopped
    /// one command would otherwise bail the *teardown* out at the driver's
    /// pre-flight check, so `quit` would never be entered at all — and what
    /// `quit` does once entered (release the pool claims, close the hosts) is
    /// pinned by its own tests in `commands/quit.rs`. What this pins is that the
    /// dispatch is not rejected before it starts.
    #[test]
    fn ctrl_d_tears_down_even_after_a_cancelled_command() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        // The state a cancelled line leaves behind.
        s.cancel_token().cancel();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);

        let (outcome, _out) =
            capturing_log(step_teardown(&r, &mut s, &mut rx), std::future::ready(()));

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert!(s.should_exit(), "the teardown ran and asked to exit");
    }

    /// A press during the teardown must **not** cancel it: cancelling the
    /// cleanup is precisely what strands the locks. The teardown runs to
    /// completion; the press only buys a warning.
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
        let (outcome, out) = capturing_log(step_teardown(&r, &mut s, &mut rx), driver);

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
    /// timeout of its own, so a blackholed refhost parks it indefinitely. Before
    /// the forwarder existed Ctrl-C killed the process there; with the handler
    /// armed, a second press has to offer the same way out or Ctrl-C would do
    /// nothing at all on this path.
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
        let (outcome, out) = capturing_log(step_teardown(&r, &mut s, &mut rx), driver);

        assert_eq!(outcome, StepOutcome::Escalate);
        assert_eq!(
            out.matches("teardown in progress").count(),
            1,
            "the first press explained itself exactly once, got: {out:?}"
        );
    }

    /// A press that raced the *previous* dispatch's completion is queued when
    /// the teardown starts; it belongs to that dispatch, not to this teardown,
    /// so the teardown must drain it. Otherwise a single genuine press during a
    /// hung teardown would force-quit with no warning at all.
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
        let (outcome, out) = capturing_log(step_teardown(&r, &mut s, &mut rx), driver);

        assert_eq!(outcome, StepOutcome::Flow(ControlFlow::Break(())));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the teardown finished");
        assert!(out.is_empty(), "stale presses warn about nothing: {out:?}");
    }

    /// The force-quit's cost is paid in the exit status, not in the process
    /// exiting from inside the loop: `Normal` returns from `main` (so every
    /// destructor runs, reedline's history flush included), `ForceQuit` carries
    /// 128 + `SIGINT` — the status a Ctrl-C death would have had.
    #[test]
    fn the_force_quit_exit_status_is_128_plus_sigint() {
        assert_eq!(ReplExit::Normal.exit_code(), None);
        assert_eq!(ReplExit::ForceQuit.exit_code(), Some(130));
    }

    /// `quit`'s `Break` survives the interruptible wrapper — the loop must still
    /// exit on it, not fall through to another prompt.
    #[test]
    fn quit_breaks_through_the_interruptible_path() {
        let (r, _) = registry();
        let (mut s, _buf) = session_with_buffer();
        let (_tx, mut rx) = mpsc::channel(INTERRUPT_QUEUE);
        let (outcome, _out) = capturing_log(
            step_interruptible(&r, &mut s, "quit", &mut rx),
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
        // clap's own "error: " prefix must survive exactly once, not doubled
        // with mtui's own level prefix.
        assert_eq!(
            out.matches("error: ").count(),
            1,
            "exactly one error prefix, got: {out:?}"
        );
        assert!(!out.contains("error: error:"), "no doubled prefix: {out:?}");
    }
}
