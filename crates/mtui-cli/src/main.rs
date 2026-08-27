//! `mtui` — the interactive REPL entry point.
//!
//! Parses the top-level [`Args`] (clap handles `--help`/`--version` — the latter
//! carrying `mtui-core`'s build-provenance block — and usage errors, exiting the
//! process itself), initialises `tracing` from `-d/--debug` + `RUST_LOG`, seeds
//! the session from `-a`/`-k` and `--sut` ([`seed_session`]), then enters the
//! REPL.
//!
//! The REPL is this binary's **only** driving surface: there is no positional
//! command mode, since headless single-command dispatch is an
//! `mtui-mcp`/embedding concern. Config resolves through
//! [`Args::resolve_config`]: the file layers via
//! [`Config::load`](mtui_config::Config::load), then the CLI overrides on top.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use clap::Parser;
use mtui_cli::{Repl, init_tracing, seed_session};
use mtui_core::{Args, ColorMode, Session, register_all};

fn main() -> anyhow::Result<()> {
    // clap auto-handles `--help`/`--version` (exit 0) and usage errors (exit 2)
    // before returning here.
    let args = Args::parse();

    // One `--color` decision drives every operator-facing level, since they all
    // flow through this single `tracing` subscriber.
    let color = ColorMode::from(args.color);
    let log_level_sink = init_tracing(args.debug, color);
    tracing::debug!(debug = args.debug, "mtui starting");

    // One runtime bridging the synchronous reedline editor to the async engine.
    // Safe to `block_on` per line: no host tasks are in flight mid-line.
    let runtime = tokio::runtime::Runtime::new()?;

    let registry = Arc::new(register_all());
    let mut session = Session::new(args.resolve_config(), true);

    // `Session::new` defaults the display to `ColorMode::Never`, so without this
    // the content color helpers never emit ANSI in the live REPL. `Auto` then
    // re-checks TTY / `NO_COLOR` / `COLOR` at each call.
    session.display.set_color(color);

    // Composition root: the REPL-only sinks. `mtui-mcp` installs none of them,
    // so there the toasts, `set_log_level` and the timeout prompt are no-ops.
    // The notify backend is itself a no-op off a TTY, so a piped REPL is safe.
    session.set_notify_sink(Box::new(|msg: &str, error: bool| {
        mtui_cli::notify_user(msg, error);
    }));

    session.set_log_level_sink(log_level_sink);

    // Installed before `seed_session`, so hosts connected during `-a` seeding
    // already carry the SSH command-timeout question ("keep waiting? [Y/n]").
    session.set_prompter(mtui_hosts::Prompter::stdin());

    // A failed explicit update exits here rather than entering an empty REPL.
    if let ControlFlow::Break(code) = runtime.block_on(seed_session(&registry, &mut session, &args))
    {
        std::process::exit(code.into());
    }

    // Shared so the reedline-owned tab completer reads the same live session the
    // loop dispatches against (see `Repl`).
    let session = Arc::new(Mutex::new(session));
    let mut repl = Repl::new(registry, session);

    let ending = runtime.block_on(repl.run())?;
    let Some(status) = ending.status() else {
        return Ok(());
    };
    // Force-quit: exactly one destructor, then leave. `into_line_editor` keeps
    // the blast radius at reedline, which persists its `FileBackedHistory` on
    // drop, without tearing down the whole session graph.
    //
    // `process::exit`, not `main() -> ExitCode`: returning drops the runtime,
    // which blocks on in-flight `spawn_blocking` — including the stdin prompter
    // very likely outstanding when someone force-quits, whose read never
    // returns.
    drop(repl.into_line_editor());
    std::process::exit(status.into());
}
