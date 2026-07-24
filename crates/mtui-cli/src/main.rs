//! `mtui` — the interactive REPL entry point.
//!
//! Parses the top-level [`Args`](mtui_core::Args) (clap handles
//! `--help`/`--version` — the latter carrying the build-provenance block baked
//! into `mtui-core` — and usage errors, exiting the process itself), initialises
//! `tracing` from `-d/--debug` + `RUST_LOG`, seeds the session from `-a`/`-k`
//! and `--sut` ([`seed_session`], the pre-`cmdloop` half of upstream
//! `run_mtui`), then enters the interactive REPL (P6.2).
//!
//! Like upstream `mtui`, this binary has exactly **one** driving surface: the
//! REPL. There is no positional command / single-command mode — headless
//! single-command dispatch is an `mtui-mcp`/embedding concern
//! ([`mtui_core::run_once`]), not a CLI mode. Full config loading + `Args` merge
//! remains later Phase-6 config work.

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
    // `tracing` subscriber, so a single color decision drives them all
    // (upstream's single `ColorFormatter`).
    let color = ColorMode::from(args.color);
    // `init_tracing` installs the subscriber behind a reload layer and hands back
    // the sink that flips the level at runtime; it is wired onto the session below
    // so `set_log_level` actually changes the live filter (upstream `log.setLevel`).
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
    // `Auto` (the default) then re-checks TTY / `NO_COLOR` / `COLOR` at each call,
    // matching upstream `colors_enabled`.
    session.display.set_color(color);

    // Composition root: wire the REPL-only desktop-notification sink to the
    // headless-safe `notification::notify_user`. `mtui-mcp` never installs it, so
    // toasts stay a REPL courtesy (upstream `prompt.notify_user`). The backend is
    // itself a no-op off a TTY / without the `notify` feature, so this is safe
    // even when the REPL runs piped.
    session.set_notify_sink(Box::new(|msg: &str, error: bool| {
        mtui_cli::notify_user(msg, error);
    }));

    // Composition root: install the REPL-only log-level sink (upstream
    // `log.setLevel`). `set_log_level` calls this to reload the `tracing`
    // subscriber's `EnvFilter` to the chosen level at runtime. `mtui-mcp` never
    // installs one, so there the command stays a log-only no-op.
    session.set_log_level_sink(log_level_sink);

    // Composition root: install the REPL-only serialised interactive prompter
    // (upstream `main.py`'s `prompter = Prompter()`). It backs the SSH
    // command-timeout question ("keep waiting? [Y/n]"), serialised across
    // parallel host tasks and suspending any live spinner. Installed *before*
    // `seed_session` so hosts connected during `-a` seeding already carry the
    // timeout prompt. `mtui-mcp` never installs one (headless → immediate
    // abort, upstream `prompter=None`).
    session.set_prompter(mtui_hosts::Prompter::stdin());

    // Seed the session from `-a`/`-k` (load the update) and `--sut` (add hosts)
    // before the loop — the pre-`cmdloop` half of upstream `run_mtui`. A failed
    // explicit update exits here rather than entering an empty REPL.
    if let ControlFlow::Break(code) = runtime.block_on(seed_session(&registry, &mut session, &args))
    {
        std::process::exit(code);
    }

    // The session and registry are shared behind `Arc`/`Arc<Mutex>` so the tab
    // completer (P6.3), owned by the reedline editor, reads the same live
    // session the loop dispatches against (see `Repl`).
    let session = Arc::new(Mutex::new(session));
    let mut repl = Repl::new(registry, session);

    runtime.block_on(repl.run())
}
