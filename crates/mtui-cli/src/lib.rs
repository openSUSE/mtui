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

use std::io::Write;

use mtui_core::{ColorMode, LogLevel, LogLevelSink};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A spinner-aware stderr writer for the `tracing` subscriber.
///
/// The Rust port of upstream's `SpinnerAwareStreamHandler`
/// (`mtui.cli.colors.formatter`): every log record is written while holding a
/// [`mtui_hosts::suspend`] guard, so a live TTY spinner erases its current frame
/// (`\r` + clear-to-EOL, homing the cursor to column 0), the record lands on a
/// clean line, and the spinner repaints on its next tick. A strict no-op beyond
/// taking the paint lock when no spinner is active — notably off a TTY, where
/// spinners never register — so this behaves exactly like a plain stderr writer
/// there.
struct SpinnerAwareStderr;

impl Write for SpinnerAwareStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Hold the suspend guard only for the synchronous write: it erases any
        // live frame first and blocks a repaint until the record is flushed.
        // Never held across an await (there is none here).
        let _quiet = mtui_hosts::suspend();
        std::io::stderr().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> MakeWriter<'a> for SpinnerAwareStderr {
    type Writer = SpinnerAwareStderr;

    fn make_writer(&'a self) -> Self::Writer {
        SpinnerAwareStderr
    }
}

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
///
/// **Runtime reload.** The `EnvFilter` is installed behind a
/// [`tracing_subscriber::reload`] layer, and the returned [`LogLevelSink`]
/// closure flips it at runtime — this is what backs the `set_log_level` command
/// (upstream `log.setLevel`). Install it on the session with
/// [`set_log_level_sink`](mtui_core::Session::set_log_level_sink). The closure
/// keeps the reload [`Handle`](tracing_subscriber::reload::Handle) inside
/// `mtui-cli`, so the `tracing_subscriber` types never leak into the lower
/// crates. A runtime `set_log_level` **replaces the whole filter** with the new
/// level (matching upstream's global `setLevel`), discarding any per-target
/// `RUST_LOG` directives the process started with. It changes the *level filter
/// only*, not the event format — a runtime switch to `debug` does not
/// retroactively add the verbose timestamp/target layout selected by `-d` at
/// startup (deliberate, consistent with [`logfmt`]).
#[must_use]
pub fn init_tracing(debug: bool, color: ColorMode) -> LogLevelSink {
    let default = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // Wrap the filter in a reload layer so `set_log_level` can flip it live.
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter);
    if debug {
        // Verbose diagnostics: keep timestamp + level + target (stock format).
        // The writer stays spinner-aware so a mid-fan-out DEBUG line still
        // erases the live frame before printing (upstream SpinnerAwareStreamHandler).
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(SpinnerAwareStderr))
            .init();
    } else {
        // Compact operator output: lowercased colored level, `level: message`,
        // no timestamp/target. Disable the subscriber's own ANSI so only the
        // custom format's explicit level coloring emits escapes; the ANSI
        // decision is shared with the display via `ColorMode::resolve`. The
        // spinner-aware writer erases any live frame before each record so
        // worker-thread log lines emitted mid-spin render flush-left.
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .event_format(logfmt::CompactLevelFormat::new(color.resolve()))
                    .with_writer(SpinnerAwareStderr),
            )
            .init();
    }

    // The sink `set_log_level` drives: reload the whole `EnvFilter` to the new
    // level. Best-effort — if the subscriber was already dropped, upstream
    // likewise just logs and moves on.
    Box::new(move |level: LogLevel| {
        let _ = handle.reload(EnvFilter::new(level_directive(level)));
    })
}

/// The `EnvFilter` directive string for a [`LogLevel`] (the lowercased
/// [`tracing::Level`] name, e.g. `"debug"`), used to rebuild the filter on a
/// runtime `set_log_level`.
fn level_directive(level: LogLevel) -> String {
    level.as_tracing().as_str().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[test]
    fn level_directive_is_lowercased_tracing_name() {
        assert_eq!(level_directive(LogLevel::Error), "error");
        assert_eq!(level_directive(LogLevel::Warning), "warn");
        assert_eq!(level_directive(LogLevel::Info), "info");
        assert_eq!(level_directive(LogLevel::Debug), "debug");
    }

    /// A `MakeWriter` over a shared buffer so a scoped subscriber's output can be
    /// inspected without touching the process-global default subscriber.
    #[derive(Clone)]
    struct BufMaker(Arc<Mutex<Vec<u8>>>);
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufMaker {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            BufWriter(Arc::clone(&self.0))
        }
    }

    /// Reloading the filter through the handle changes which events pass, exactly
    /// as the sink `init_tracing` installs does. Mirrors the reload wiring
    /// (`reload::Layer` around an `EnvFilter`, `handle.reload(...)`) on a scoped
    /// subscriber so it does not touch the process-global default.
    #[test]
    fn reload_handle_changes_active_level_at_runtime() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        // Start at `info`: a `debug!` must be filtered out.
        let (filter, handle) =
            tracing_subscriber::reload::Layer::new(EnvFilter::new(level_directive(LogLevel::Info)));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(BufMaker(Arc::clone(&buf))),
        );

        // The closure is the same shape as the one `init_tracing` returns.
        let mut sink: LogLevelSink = Box::new(move |level: LogLevel| {
            let _ = handle.reload(EnvFilter::new(level_directive(level)));
        });

        with_default(subscriber, || {
            tracing::debug!("hidden at info");
            tracing::info!("visible at info");
            // Flip to debug at runtime.
            sink(LogLevel::Debug);
            tracing::debug!("visible at debug");
            // Flip to error: info now suppressed.
            sink(LogLevel::Error);
            tracing::info!("hidden at error");
            tracing::error!("visible at error");
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!out.contains("hidden at info"), "got: {out:?}");
        assert!(out.contains("visible at info"), "got: {out:?}");
        assert!(out.contains("visible at debug"), "got: {out:?}");
        assert!(!out.contains("hidden at error"), "got: {out:?}");
        assert!(out.contains("visible at error"), "got: {out:?}");
    }
}
