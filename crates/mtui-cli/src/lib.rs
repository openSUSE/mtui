//! `mtui-cli` — the interactive REPL library behind the `mtui` binary.
//!
//! The binary ([`main.rs`](../main.rs)) is a thin shell: it parses the
//! top-level args, builds the [`Session`](mtui_core::Session) and command
//! [`Registry`](mtui_core::Registry), and drives [`Repl::run`]. Exposing the
//! REPL as a library lets the `tests/**` suite exercise
//! the loop's `repl::step` seam without a TTY.

pub mod completer;
pub(crate) mod edit;
pub mod highlighter;
pub(crate) mod history;
pub mod logfmt;
pub mod notification;
pub mod prompt;
pub mod repl;
pub mod shell;
pub mod startup;

pub use notification::notify_user;
pub use prompt::MtuiPrompt;
pub use repl::{Repl, ReplExit};
pub use startup::seed_session;

use std::io::Write;

use mtui_core::{ColorMode, LogLevel, LogLevelSink, TRANSPORT_LOG_CARVE_OUT};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A spinner-aware stderr writer for the `tracing` subscriber.
///
/// Every log record is written while holding a [`mtui_hosts::suspend`] guard,
/// so a live TTY spinner erases its current frame
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
/// Honours `RUST_LOG` (mtui logging contract); `-d/--debug` raises the
/// default level to `DEBUG` when `RUST_LOG` is unset.
///
/// **`DEBUG` raises mtui's targets only.** Both `-d` and a runtime
/// `set_log_level debug` build their directive through `level_directive`,
/// which caps the third-party HTTP transport (`hyper_util`, `hyper`, `reqwest`)
/// at `INFO`: those crates log connection details — including a pool authority
/// that can carry redirect-supplied userinfo — at `DEBUG` (#439). An operator
/// who needs the transport's own view opts in explicitly with `RUST_LOG`
/// (e.g. `RUST_LOG=hyper_util=debug`), which replaces these defaults entirely.
///
/// At the **default** level the output is compact and colorized like the
/// command display: a lowercased,
/// colored level token (green `info` / yellow `warn` / red `error`) then
/// `": "` then the message — no timestamp, no module target (see
/// [`logfmt::CompactLevelFormat`]). Whether escapes are emitted is resolved from
/// `color` via the *same* [`ColorMode::resolve`] the display uses, so
/// `--color auto/always/never` governs the level token and the `error:` line
/// identically.
///
/// Under `-d/--debug` the full verbose Rust format is kept (timestamp + level +
/// target, e.g. `2026-07-10T09:41:39.891821Z DEBUG mtui_cli::repl: …`) for
/// diagnostics; the compact colored layer is not applied there.
///
/// The DEBUG-only `" [module:function]"` suffix is not reproduced — under `-d`
/// the verbose format restores the module `target`, which covers the
/// diagnostic need.
///
/// The user-facing *command error* is rendered by the session display, not this
/// subscriber (see `repl::render_error`), so a failing command never prints
/// twice.
///
/// **Runtime reload.** The `EnvFilter` is installed behind a
/// [`tracing_subscriber::reload`] layer, and the returned [`LogLevelSink`]
/// closure flips it at runtime — this is what backs the `set_log_level`
/// command. Install it on the session with
/// [`set_log_level_sink`](mtui_core::Session::set_log_level_sink). The closure
/// keeps the reload [`Handle`](tracing_subscriber::reload::Handle) inside
/// `mtui-cli`, so the `tracing_subscriber` types never leak into the lower
/// crates. A runtime `set_log_level` **replaces the whole filter** with the new
/// level, discarding any per-target `RUST_LOG` directives the process started
/// with — including an explicit transport opt-in, which the reloaded `debug`
/// directive replaces with the carve-out above. It changes the *level filter
/// only*, not the event format — a runtime switch to `debug` does not
/// retroactively add the verbose timestamp/target layout selected by `-d` at
/// startup (deliberate, consistent with [`logfmt`]).
#[must_use]
pub fn init_tracing(debug: bool, color: ColorMode) -> LogLevelSink {
    // Both startup levels go through the same helper the `set_log_level` sink
    // uses, so the transport carve-out cannot apply to one path and not the other.
    let startup = if debug {
        LogLevel::Debug
    } else {
        LogLevel::Info
    };
    let default = level_directive(startup);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // Wrap the filter in a reload layer so `set_log_level` can flip it live.
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter);
    if debug {
        // Verbose diagnostics: keep timestamp + level + target (stock format).
        // The writer stays spinner-aware so a mid-fan-out DEBUG line still
        // erases the live frame before printing.
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
    // level. Best-effort — if the subscriber was already dropped, the reload is
    // silently ignored.
    Box::new(move |level: LogLevel| {
        let _ = handle.reload(EnvFilter::new(level_directive(level)));
    })
}

/// The `EnvFilter` directive string for a [`LogLevel`]: the lowercased
/// [`tracing::Level`] name (e.g. `"debug"`), used both to seed the startup
/// fallback filter and to rebuild it on a runtime `set_log_level`.
///
/// At `debug` the base level is followed by [`TRANSPORT_LOG_CARVE_OUT`], which
/// holds the third-party HTTP stack at `INFO` — raising mtui's verbosity must
/// not print hyper-util's connection-pool key, whose authority can carry
/// redirect-supplied userinfo (#439). The coarser levels stay bare: appending
/// the carve-out there would *raise* those targets to `INFO` above an operator's
/// chosen `error`/`warn`.
fn level_directive(level: LogLevel) -> String {
    let base = level.as_tracing().as_str().to_ascii_lowercase();
    match level {
        LogLevel::Debug => format!("{base},{TRANSPORT_LOG_CARVE_OUT}"),
        LogLevel::Error | LogLevel::Warning | LogLevel::Info => base,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// Byte-pins the directive strings themselves: the bare lowercased
    /// `tracing` name at the coarser levels, and `debug` plus the transport
    /// carve-out (#439) spelled out in full. These literals — not the
    /// behavioural test below — are what catch a respelling that `EnvFilter`
    /// still happens to cover (`hyper-util` for `hyper_util`, which the
    /// `hyper=info` prefix match would silently absorb).
    #[test]
    fn level_directive_pins_bare_levels_and_debug_transport_carve_out() {
        // The coarser levels are the bare `tracing` name: appending the
        // transport carve-out there would *raise* those targets to `info`
        // above an operator's chosen `error`/`warn`.
        assert_eq!(level_directive(LogLevel::Error), "error");
        assert_eq!(level_directive(LogLevel::Warning), "warn");
        assert_eq!(level_directive(LogLevel::Info), "info");
        // `debug` carries the third-party transport carve-out (#439): hyper-util
        // logs its pool key — authority userinfo included — at DEBUG. Spelled
        // out literally, never rebuilt from `TRANSPORT_LOG_CARVE_OUT`, so
        // emptying that constant cannot green both sides at once.
        assert_eq!(
            level_directive(LogLevel::Debug),
            "debug,hyper_util=info,hyper=info,reqwest=info"
        );
    }

    /// A runtime `set_log_level debug` must raise mtui's own targets to DEBUG
    /// without switching on the HTTP transport's DEBUG logging, where a
    /// credential-bearing pool authority would be printed (#439). Exercises the
    /// real sink shape (`handle.reload(EnvFilter::new(level_directive(level)))`)
    /// on a scoped subscriber, so it pins the assembled directive string's
    /// *behaviour*, not just its bytes — an `EnvFilter`-invalid target would be
    /// dropped silently by `EnvFilter::new` and a pure string pin would not
    /// notice.
    #[test]
    fn set_log_level_debug_does_not_enable_transport_debug() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (filter, handle) =
            tracing_subscriber::reload::Layer::new(EnvFilter::new(level_directive(LogLevel::Info)));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(BufMaker(Arc::clone(&buf))),
        );

        let mut sink: LogLevelSink = Box::new(move |level: LogLevel| {
            let _ = handle.reload(EnvFilter::new(level_directive(level)));
        });

        with_default(subscriber, || {
            sink(LogLevel::Debug);
            // The leak shape from hyper-util 0.1.20 (`pool.rs:401`), verbatim.
            tracing::debug!(
                target: "hyper_util::client::legacy::pool",
                "pooling idle connection for (\"http\", alice:s3cret@example.test:9)"
            );
            tracing::debug!(target: "mtui_cli::probe", "mtui debug reaches the log");
            tracing::info!(
                target: "hyper_util::client::legacy::pool",
                "transport info reaches the log"
            );
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        // The guard: a unique credential-shaped token, so no unrelated line can
        // satisfy (or vacuously fail) the assertion.
        assert!(
            !out.contains("s3cret"),
            "transport DEBUG must stay filtered, got: {out:?}"
        );
        // Anti-vacuity: without this, assertion 1 would also pass with the whole
        // filter stuck at `info` (i.e. with `set_log_level debug` broken).
        assert!(
            out.contains("mtui debug reaches the log"),
            "mtui targets must still reach DEBUG, got: {out:?}"
        );
        // The cap is exactly `info`, not `warn`/`off`: real transport problems
        // must still reach the operator.
        assert!(
            out.contains("transport info reaches the log"),
            "transport INFO must survive the carve-out, got: {out:?}"
        );
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
