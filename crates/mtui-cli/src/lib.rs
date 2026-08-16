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

use mtui_core::{
    ColorMode, LogLevel, LogLevelSink, TRANSPORT_LOG_CARVE_OUT, resolve_log_directives,
};
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
/// **`DEBUG` raises mtui's targets only — by whichever knob turned it on.**
/// `-d` and a runtime `set_log_level debug` build their directive through
/// `level_directive`, which appends [`TRANSPORT_LOG_CARVE_OUT`]; a `RUST_LOG`
/// gets the same cap applied *on top* of the operator's directives by
/// [`resolve_log_directives`], so `RUST_LOG=debug` — the ordinary way anyone
/// turns on debug logging — no longer switches the third-party HTTP transport's
/// `DEBUG` back on. Those crates log connection details, including a pool
/// authority that can carry redirect-supplied userinfo, at `DEBUG` (#439).
/// Naming a transport target (`RUST_LOG=hyper_util=debug`) is an informed
/// opt-in: it is honoured verbatim, and announced on stderr at startup.
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
    let (filter, notice) = startup_filter(debug);
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

    if let Some(notice) = notice {
        // Straight to stderr, not `tracing::warn!`: the opt-in that triggers
        // this is typically `RUST_LOG=hyper_util=debug`, which enables no
        // `mtui_*` target at all, so a `WARN` event would be swallowed by the
        // very filter it is warning about.
        eprintln!("{notice}");
    }

    // The sink `set_log_level` drives: reload the whole `EnvFilter` to the new
    // level. Best-effort — if the subscriber was already dropped, the reload is
    // silently ignored.
    Box::new(move |level: LogLevel| {
        let _ = handle.reload(EnvFilter::new(level_directive(level)));
    })
}

/// The startup `EnvFilter` and the optional one-line stderr notice, resolved
/// from `$RUST_LOG` and this process's own defaults.
///
/// The seam `init_tracing` resolves through, so the `RUST_LOG` composition is
/// testable without installing a global subscriber (which a process can only do
/// once). The defaults come from [`level_directive`], so the `-d` fallback and a
/// runtime `set_log_level debug` cannot diverge; `RUST_LOG` is layered on by
/// [`resolve_log_directives`], the same helper `mtui-mcp` uses, so neither
/// entrypoint can grow its own answer.
fn startup_filter(debug: bool) -> (EnvFilter, Option<&'static str>) {
    let defaults = level_directive(if debug {
        LogLevel::Debug
    } else {
        LogLevel::Info
    });
    let resolved = resolve_log_directives(&defaults);
    match EnvFilter::try_new(&resolved.directives) {
        Ok(filter) => (filter, resolved.notice()),
        // A malformed `RUST_LOG` falls back to the defaults, exactly as the
        // previous `EnvFilter::try_from_default_env()` did — and the defaults
        // cap the transport, so the opt-in notice falls away with it.
        Err(_) => (EnvFilter::new(&defaults), None),
    }
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

    /// `RUST_LOG=debug` — the single most common way anyone turns on debug
    /// logging — must **not** re-open the transport leak the carve-out exists to
    /// close (#439). It names no transport target, so the cap is layered on top
    /// of it and mtui's own DEBUG still flows.
    ///
    /// Note `startup_filter(false)`: `-d` is *not* set, so everything asserted
    /// here is the `RUST_LOG` path, not the default path the byte-pin above
    /// already covers.
    #[test]
    #[serial_test::serial(env)]
    fn rust_log_debug_still_holds_the_transport_at_info() {
        let (filter, notice) = with_rust_log(Some("debug"), || startup_filter(false));
        let out = probe(filter);

        assert!(
            !out.contains("s3cret"),
            "RUST_LOG=debug must not enable the transport's DEBUG, got: {out:?}"
        );
        // Anti-vacuity: assertion 1 also passes with the filter stuck at `info`,
        // i.e. with `RUST_LOG` ignored altogether.
        assert!(
            out.contains("mtui debug reaches the log"),
            "RUST_LOG=debug must still raise mtui's own targets, got: {out:?}"
        );
        assert!(
            out.contains("transport info reaches the log"),
            "transport INFO must survive the carve-out, got: {out:?}"
        );
        assert_eq!(notice, None, "nothing was opted into, so nothing to say");
    }

    /// The informed opt-in stays open: naming a transport target hands the
    /// operator the transport's own view, verbatim — and says so on stderr,
    /// because this is now the only way to print a pool authority.
    #[test]
    #[serial_test::serial(env)]
    fn rust_log_transport_opt_in_is_honoured_and_announced() {
        let (filter, notice) = with_rust_log(Some("hyper_util=debug"), || startup_filter(false));
        let out = probe(filter);

        assert!(
            out.contains("s3cret"),
            "an explicit hyper_util=debug must not be capped, got: {out:?}"
        );
        assert_eq!(notice, Some(mtui_core::TRANSPORT_DEBUG_NOTICE));
    }

    /// An unrelated per-target directive must not read as a transport opt-in.
    /// The `debug` beside it is what would reach `hyper_util`, and it is still
    /// capped; `mtui_cli=trace` still gets its TRACE.
    #[test]
    #[serial_test::serial(env)]
    fn rust_log_unrelated_target_keeps_the_transport_capped() {
        let (filter, notice) =
            with_rust_log(Some("mtui_cli=trace,debug"), || startup_filter(false));
        let out = probe(filter);

        assert!(
            !out.contains("s3cret"),
            "an unrelated target directive must not stand the carve-out down, got: {out:?}"
        );
        assert!(
            out.contains("mtui trace reaches the log"),
            "the operator's own directive must still be honoured, got: {out:?}"
        );
        assert_eq!(notice, None);
    }

    /// A `RUST_LOG` `EnvFilter` cannot parse falls back to the (capped)
    /// defaults, as `try_from_default_env` did — and the opt-in notice falls
    /// away with the opt-in, even though the unparseable value named a
    /// transport target.
    #[test]
    #[serial_test::serial(env)]
    fn malformed_rust_log_falls_back_to_the_capped_defaults() {
        let (filter, notice) = with_rust_log(Some("hyper_util=debug,!!!"), || startup_filter(true));
        let out = probe(filter);

        assert!(
            !out.contains("s3cret"),
            "the fallback defaults cap the transport, got: {out:?}"
        );
        assert!(
            out.contains("mtui debug reaches the log"),
            "the fallback is the `-d` default, which is DEBUG, got: {out:?}"
        );
        assert_eq!(notice, None, "a discarded opt-in must not be announced");
    }

    /// With `RUST_LOG` unset the entrypoint's own defaults apply unchanged —
    /// the `-d` arm still reaching mtui's DEBUG and still capping the transport.
    #[test]
    #[serial_test::serial(env)]
    fn unset_rust_log_uses_the_debug_defaults() {
        let (filter, notice) = with_rust_log(None, || startup_filter(true));
        let out = probe(filter);

        assert!(
            !out.contains("s3cret"),
            "the `-d` default caps the transport, got: {out:?}"
        );
        assert!(
            out.contains("mtui debug reaches the log"),
            "`-d` must reach mtui's DEBUG, got: {out:?}"
        );
        assert_eq!(notice, None);
    }

    /// Run `body` with `$RUST_LOG` set (or removed), restoring the previous
    /// value afterwards. Callers must hold `#[serial(env)]`: the whole crate's
    /// unit tests share one process, so the variable is a process-global.
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // `#[serial(env)]` guard on every caller makes the mutation exclusive.
    #[allow(unsafe_code)]
    fn with_rust_log<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let previous = std::env::var("RUST_LOG").ok();
        // SAFETY: serialised via `#[serial(env)]`, so no other test observes or
        // mutates the environment concurrently. `set_var`/`remove_var` are
        // `unsafe` in edition 2024 for exactly that reason.
        unsafe {
            match value {
                Some(value) => std::env::set_var("RUST_LOG", value),
                None => std::env::remove_var("RUST_LOG"),
            }
        }
        let out = body();
        // SAFETY: still inside the `#[serial(env)]` critical section.
        unsafe {
            match previous {
                Some(previous) => std::env::set_var("RUST_LOG", previous),
                None => std::env::remove_var("RUST_LOG"),
            }
        }
        out
    }

    /// Emit the four probe events under `filter` on a **scoped** subscriber
    /// (the process-global default is installed once per process and cannot be
    /// replaced) and return everything that reached the writer.
    ///
    /// The transport line is the leak shape from hyper-util 0.1.20
    /// (`pool.rs:401`), verbatim; `s3cret` is a token no other line carries, so
    /// the "must not appear" assertion cannot be satisfied by the wrong record.
    fn probe(filter: EnvFilter) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(BufMaker(Arc::clone(&buf))),
        );
        with_default(subscriber, || {
            tracing::debug!(
                target: "hyper_util::client::legacy::pool",
                "pooling idle connection for (\"http\", alice:s3cret@example.test:9)"
            );
            tracing::debug!(target: "mtui_cli::probe", "mtui debug reaches the log");
            tracing::trace!(target: "mtui_cli::probe", "mtui trace reaches the log");
            tracing::info!(
                target: "hyper_util::client::legacy::pool",
                "transport info reaches the log"
            );
        });
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
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
