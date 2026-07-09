//! `mtui` — interactive REPL + non-interactive single-command entry point.
//!
//! P6.1 skeleton: parse the top-level [`Args`](mtui_core::Args) (clap handles
//! `--help`/`--version` — the latter carrying the build-provenance block baked
//! into `mtui-core` — and usage errors, exiting the process itself), initialise
//! `tracing` from `-d/--debug` + `RUST_LOG`, then bail at the point the
//! interactive REPL will occupy. The REPL read loop and engine dispatch land in
//! P6.2; non-interactive single-command mode lands in P6.7.

use clap::Parser;
use mtui_core::Args;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // Layer 1 (app invocation). clap auto-handles `--help`/`--version` (exit 0)
    // and usage errors (exit 2) before returning here.
    let args = Args::parse();

    init_tracing(args.debug);
    tracing::debug!(debug = args.debug, "mtui starting");

    // The REPL (P6.2) and non-interactive single-command dispatch (P6.7) do not
    // exist yet. Fail loudly rather than silently exiting so an accidental
    // interactive invocation is visible; P6.2 replaces this with the read loop.
    anyhow::bail!("interactive REPL not yet implemented (Phase 6.2)")
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
