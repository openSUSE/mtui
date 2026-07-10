//! `mtui-cli` — the interactive REPL library behind the `mtui` binary.
//!
//! The binary ([`main.rs`](../main.rs)) is a thin shell: it parses the
//! top-level args, builds the [`Session`](mtui_core::Session) and command
//! [`Registry`](mtui_core::Registry), and drives [`Repl::run`]. Exposing the
//! REPL as a library lets the `tests/**` suite (and the P6.8 test task) exercise
//! the loop's [`step`](repl::step) seam without a TTY.

pub mod completer;
pub mod edit;
pub mod highlighter;
pub mod history;
pub mod logfmt;
pub mod notification;
pub mod prompt;
pub mod repl;
pub mod shell;
pub mod startup;

pub use completer::MtuiCompleter;
pub use edit::{is_edit_line, run_edit};
pub use highlighter::MtuiHighlighter;
pub use history::file_backed_history;
pub use notification::{display, notify_user};
pub use prompt::MtuiPrompt;
pub use repl::{Repl, step};
pub use shell::{is_shell_line, run_shell};
pub use startup::seed_session;

use mtui_core::ColorMode;
use tracing_subscriber::EnvFilter;

/// Initialises the `tracing` subscriber.
///
/// Honours `RUST_LOG` (mtui-rs logging contract); `-d/--debug` raises the
/// default level to `DEBUG` when `RUST_LOG` is unset, mirroring upstream's
/// `if args.debug: logger.setLevel(DEBUG)`.
///
/// Format mirrors upstream's `ColorFormatter`. At the **default** level the
/// output is compact and colorized like the command display: a lowercased,
/// colored level token (green `info` / yellow `warn` / red `error`) then
/// `": "` then the message — no timestamp, no module target (see
/// [`logfmt::CompactLevelFormat`]). Whether escapes are emitted is resolved from
/// `color` via the *same* [`ColorMode::resolve`] the display uses, so
/// `--color auto/always/never` governs the level token and the `error:` line
/// identically (`mtui-rs-ilt`).
///
/// Under `-d/--debug` the full verbose Rust format is kept (timestamp + level +
/// target, e.g. `2026-07-10T09:41:39.891821Z DEBUG mtui_cli::repl: …`) for
/// diagnostics; the compact colored layer is not applied there.
///
/// **Deviation from upstream:** the DEBUG-only `" [module:function]"` suffix is
/// not reproduced — under `-d` the verbose format restores the module `target`,
/// which covers the diagnostic need.
///
/// The user-facing *command error* is rendered by the session display, not this
/// subscriber (see `repl::render_error`), so a failing command never prints
/// twice.
pub fn init_tracing(debug: bool, color: ColorMode) {
    let default = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    if debug {
        // Verbose diagnostics: keep timestamp + level + target (stock format).
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        // Compact operator output: lowercased colored level, `level: message`,
        // no timestamp/target. Disable the subscriber's own ANSI so only the
        // custom format's explicit level coloring emits escapes; the ANSI
        // decision is shared with the display via `ColorMode::resolve`.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .event_format(logfmt::CompactLevelFormat::new(color.resolve()))
            .with_writer(std::io::stderr)
            .init();
    }
}
