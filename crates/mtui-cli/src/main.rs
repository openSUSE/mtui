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
use mtui_cli::{Repl, seed_session};
use mtui_config::Config;
use mtui_core::{Args, Session, register_all};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Layer 1 (app invocation). clap auto-handles `--help`/`--version` (exit 0)
    // and usage errors (exit 2) before returning here.
    let args = Args::parse();

    init_tracing(args.debug);
    tracing::debug!(debug = args.debug, "mtui starting");

    // Bridge the synchronous reedline editor to the async engine on one runtime.
    // A per-line `block_on` inside the loop keeps the sync/async seam explicit
    // and single-sited (see PLAN risk: sync editor ↔ async dispatch); no host
    // tasks are in flight mid-line in the REPL, so there is no deadlock.
    let runtime = tokio::runtime::Runtime::new()?;

    // Full config loading + `Args` merge is Phase-6 config work; P6.2 uses the
    // defaults so the REPL is usable.
    let registry = Arc::new(register_all());
    let mut session = Session::new(Config::default(), true);

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

/// Initialises the `tracing` subscriber.
///
/// Honours `RUST_LOG` (mtui-rs logging contract); `-d/--debug` raises the
/// default level to `DEBUG` when `RUST_LOG` is unset, mirroring upstream's
/// `if args.debug: logger.setLevel(DEBUG)`.
fn init_tracing(debug: bool) {
    let default = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
