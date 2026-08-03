//! `mtui` — the interactive REPL entry point.
//!
//! Parses the top-level [`Args`] (clap handles
//! `--help`/`--version` — the latter carrying the build-provenance block baked
//! into `mtui-core` — and usage errors, exiting the process itself), initialises
//! `tracing` from `-d/--debug` + `RUST_LOG`, seeds the session from `-a`/`-k`
//! and `--sut` ([`seed_session`]), then enters the interactive REPL.
//!
//! This binary has exactly **one** driving surface: the
//! REPL. There is no positional command / single-command mode — headless
//! single-command dispatch is an `mtui-mcp`/embedding concern, not a CLI mode.
//! Config is fully resolved via [`Args::resolve_config`]: the file layers via
//! [`Config::load`](mtui_config::Config::load), then the CLI overrides
//! overlaid on top.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use clap::Parser;
use mtui_cli::{Repl, init_tracing, seed_session};
use mtui_core::{Args, ColorMode, Session, register_all};

fn main() -> anyhow::Result<()> {
    // Layer 1 (app invocation). clap auto-handles `--help`/`--version` (exit 0)
    // and usage errors (exit 2) before returning here.
    let args = Args::parse();

    // The `--color` choice resolves once into a `ColorMode`. In the REPL every
    // operator-facing level — `error`/`warn`/`info` — flows through this one
    // `tracing` subscriber, so a single color decision drives them all.
    let color = ColorMode::from(args.color);
    // `init_tracing` installs the subscriber behind a reload layer and hands back
    // the sink that flips the level at runtime; it is wired onto the session below
    // so `set_log_level` actually changes the live filter.
    let log_level_sink = init_tracing(args.debug, color);
    tracing::debug!(debug = args.debug, "mtui starting");

    // Bridge the synchronous reedline editor to the async engine on one runtime.
    // A per-line `block_on` inside the loop keeps the sync/async seam explicit
    // and single-sited (see PLAN risk: sync editor ↔ async dispatch); no host
    // tasks are in flight mid-line in the REPL, so there is no deadlock.
    let runtime = tokio::runtime::Runtime::new()?;

    // Resolve the config: the file chain (/etc → ~/.mtui.toml → XDG mtui.toml,
    // or the single --config/$MTUI_CONF file) merged, then the CLI overrides
    // (--template-dir, --connection-timeout, --gitea-token, --ssl-verify)
    // overlaid on top so command-line flags win over every config file.
    let registry = Arc::new(register_all());
    let mut session = Session::new(args.resolve_config(), true);

    // Apply the resolved `--color` to the display. `Session::new` defaults the
    // display to `ColorMode::Never`; without this the message-content color
    // helpers (`display.green/red/yellow/blue`) never emit ANSI in the live REPL.
    // `Auto` (the default) then re-checks TTY / `NO_COLOR` / `COLOR` at each call.
    session.display.set_color(color);

    // Composition root: wire the REPL-only desktop-notification sink to the
    // headless-safe `notification::notify_user`. `mtui-mcp` never installs it, so
    // toasts stay a REPL courtesy. The backend is itself a no-op off a TTY /
    // without the `notify` feature, so this is safe even when the REPL runs
    // piped.
    session.set_notify_sink(Box::new(|msg: &str, error: bool| {
        mtui_cli::notify_user(msg, error);
    }));

    // Composition root: install the REPL-only log-level sink. `set_log_level`
    // calls this to reload the `tracing` subscriber's `EnvFilter` to the chosen
    // level at runtime. `mtui-mcp` never installs one, so there the command
    // stays a log-only no-op.
    session.set_log_level_sink(log_level_sink);

    // Composition root: install the REPL-only serialised interactive prompter.
    // It backs the SSH command-timeout question ("keep waiting? [Y/n]"),
    // serialised across parallel host tasks and suspending any live spinner.
    // Installed *before* `seed_session` so hosts connected during `-a` seeding
    // already carry the timeout prompt. `mtui-mcp` never installs one
    // (headless → immediate abort).
    session.set_prompter(mtui_hosts::Prompter::stdin());

    // Seed the session from `-a`/`-k` (load the update) and `--sut` (add hosts)
    // before the loop. A failed explicit update exits here rather than
    // entering an empty REPL.
    if let ControlFlow::Break(code) = runtime.block_on(seed_session(&registry, &mut session, &args))
    {
        std::process::exit(code.into());
    }

    // The session and registry are shared behind `Arc`/`Arc<Mutex>` so the tab
    // completer, owned by the reedline editor, reads the same live
    // session the loop dispatches against (see `Repl`).
    let session = Arc::new(Mutex::new(session));
    let mut repl = Repl::new(registry, session);

    runtime.block_on(repl.run())
}
