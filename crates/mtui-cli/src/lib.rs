//! `mtui-cli` — the interactive REPL library behind the `mtui` binary.
//!
//! The binary ([`main.rs`](../main.rs)) is a thin shell: it parses the
//! top-level args, builds the [`Session`](mtui_core::Session) and command
//! [`Registry`](mtui_core::Registry), and drives [`Repl::run`]. Exposing the
//! REPL as a library lets `tests/**` exercise the loop's `repl::step` seam
//! without a TTY.

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
/// Every record is written under a [`mtui_hosts::suspend`] guard, so a live TTY
/// spinner erases its frame (`\r` + clear-to-EOL), the record lands on a clean
/// line, and the spinner repaints on its next tick. With no spinner active —
/// notably off a TTY — this is a plain stderr writer plus a paint-lock take.
struct SpinnerAwareStderr;

impl Write for SpinnerAwareStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Held only for the synchronous write, never across an await.
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
/// Honours `RUST_LOG`; `-d/--debug` raises the default level to `DEBUG` when
/// `RUST_LOG` is unset.
///
/// **`DEBUG` raises mtui's targets only — by whichever knob turned it on.** `-d`
/// and a runtime `set_log_level debug` go through `level_directive`, which
/// appends [`TRANSPORT_LOG_CARVE_OUT`]; [`resolve_log_directives`] applies the
/// same cap *on top* of a `RUST_LOG`, so `RUST_LOG=debug` cannot switch the
/// third-party HTTP transport's `DEBUG` back on — those crates log a connection
/// pool authority that can carry redirect-supplied userinfo (#439). Naming a
/// transport target (`RUST_LOG=hyper_util=debug`) is an informed opt-in:
/// honoured verbatim, and announced on stderr at startup.
///
/// At the **default** level the output is a lowercased colored level token,
/// `": "` and the message ([`logfmt::CompactLevelFormat`]); escapes resolve from
/// `color` through the *same* [`ColorMode::resolve`] the display uses, so
/// `--color` governs both identically. `-d/--debug` keeps the verbose Rust
/// format instead, and mtui deliberately has no DEBUG-only
/// `" [module:function]"` suffix since that format's `target` covers the need.
/// The user-facing *command error* is rendered by the session display, not this
/// subscriber (`repl::render_error`), so a failing command never prints twice.
///
/// **Runtime reload.** The `EnvFilter` sits behind a
/// [`tracing_subscriber::reload`] layer and the returned [`LogLevelSink`] flips
/// it, backing the `set_log_level` command; install it with
/// [`set_log_level_sink`](mtui_core::Session::set_log_level_sink). Keeping the
/// [`Handle`](tracing_subscriber::reload::Handle) inside the closure keeps
/// `tracing_subscriber` out of the lower crates. A reload **replaces the whole
/// filter**, discarding the per-target `RUST_LOG` directives the process started
/// with — an explicit transport opt-in included, which `debug` replaces with the
/// carve-out above. It changes the *level filter only*, never the event format:
/// switching to `debug` at runtime does not retroactively add `-d`'s verbose
/// layout (deliberate, consistent with [`logfmt`]).
#[must_use = "install the sink via Session::set_log_level_sink, or `set_log_level` has no effect"]
pub fn init_tracing(debug: bool, color: ColorMode) -> LogLevelSink {
    let (filter, notice) = startup_filter(debug);
    // Wrap the filter in a reload layer so `set_log_level` can flip it live.
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter);
    if debug {
        // Stock verbose format, but still spinner-aware so a mid-fan-out DEBUG
        // line erases the live frame before printing.
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(SpinnerAwareStderr))
            .init();
    } else {
        // The subscriber's own ANSI is off so only the custom format's explicit
        // level coloring emits escapes, keeping the decision shared with the
        // display via `ColorMode::resolve`.
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
        // Not `tracing::warn!`: the triggering opt-in (`hyper_util=debug`)
        // enables no `mtui_*` target, so the event would be swallowed by the
        // very filter it warns about.
        eprintln!("{notice}");
    }

    // Best-effort: a reload after the subscriber was dropped is ignored.
    Box::new(move |level: LogLevel| {
        let _ = handle.reload(EnvFilter::new(level_directive(level)));
    })
}

/// The startup `EnvFilter` and the optional one-line stderr notice, resolved
/// from `$RUST_LOG` and this process's own defaults.
///
/// A seam, so the `RUST_LOG` composition is testable without installing a global
/// subscriber (once-per-process). The defaults come from [`level_directive`], so
/// the `-d` fallback and a runtime `set_log_level debug` cannot diverge, and
/// `RUST_LOG` is layered on by [`resolve_log_directives`], the same helper
/// `mtui-mcp` uses, so neither entrypoint grows its own answer.
fn startup_filter(debug: bool) -> (EnvFilter, Option<&'static str>) {
    let defaults = level_directive(if debug {
        LogLevel::Debug
    } else {
        LogLevel::Info
    });
    let resolved = resolve_log_directives(&defaults);
    match EnvFilter::try_new(&resolved.directives) {
        Ok(filter) => (filter, resolved.notice()),
        // A malformed `RUST_LOG` falls back to the defaults, which cap the
        // transport — so the opt-in notice falls away with the opt-in.
        Err(_) => (EnvFilter::new(&defaults), None),
    }
}

/// The `EnvFilter` directive string for a [`LogLevel`] — the lowercased
/// [`tracing::Level`] name — seeding the startup fallback filter and rebuilt on
/// a runtime `set_log_level`.
///
/// At `debug` the base level is followed by [`TRANSPORT_LOG_CARVE_OUT`], holding
/// the third-party HTTP stack at `INFO`: raising mtui's verbosity must not print
/// hyper-util's connection-pool key, whose authority can carry
/// redirect-supplied userinfo (#439). The coarser levels stay bare — appending
/// the carve-out would *raise* those targets above an operator's `error`/`warn`.
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

    /// Byte-pins the directive strings. These literals — not the behavioural
    /// test below — catch a respelling that `EnvFilter` still happens to cover
    /// (`hyper-util` for `hyper_util`, silently absorbed by the `hyper=info`
    /// prefix match).
    #[test]
    fn level_directive_pins_bare_levels_and_debug_transport_carve_out() {
        assert_eq!(level_directive(LogLevel::Error), "error");
        assert_eq!(level_directive(LogLevel::Warning), "warn");
        assert_eq!(level_directive(LogLevel::Info), "info");
        // The #439 carve-out, spelled out rather than rebuilt from
        // `TRANSPORT_LOG_CARVE_OUT`, so emptying that constant cannot green
        // both sides at once.
        assert_eq!(
            level_directive(LogLevel::Debug),
            "debug,hyper_util=info,hyper=info,reqwest=info"
        );
    }

    /// A runtime `set_log_level debug` must raise mtui's own targets without
    /// switching on the transport's DEBUG, which prints a credential-bearing
    /// pool authority (#439). Runs the real sink shape on a scoped subscriber so
    /// it pins the directive's *behaviour*: an `EnvFilter`-invalid target is
    /// dropped silently, which a pure string pin would not notice.
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
        // `s3cret` is unique to the transport line, so no unrelated record can
        // satisfy (or vacuously fail) this.
        assert!(
            !out.contains("s3cret"),
            "transport DEBUG must stay filtered, got: {out:?}"
        );
        // Anti-vacuity: assertion 1 also passes with the filter stuck at `info`.
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

    /// `RUST_LOG=debug` — the commonest way to turn on debug logging — must not
    /// re-open the #439 transport leak: it names no transport target, so the cap
    /// layers on top while mtui's own DEBUG still flows. `startup_filter(false)`
    /// keeps this on the `RUST_LOG` path, not the default the byte-pin covers.
    #[test]
    #[serial_test::serial(env)]
    fn rust_log_debug_still_holds_the_transport_at_info() {
        let (filter, notice) = with_rust_log(Some("debug"), || startup_filter(false));
        let out = probe(filter);

        assert!(
            !out.contains("s3cret"),
            "RUST_LOG=debug must not enable the transport's DEBUG, got: {out:?}"
        );
        // Anti-vacuity: assertion 1 also passes with `RUST_LOG` ignored.
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

    /// The informed opt-in stays open: naming a transport target hands over the
    /// transport's own view verbatim, and says so on stderr, because it is the
    /// only way left to print a pool authority.
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

    /// An unparseable `RUST_LOG` falls back to the capped defaults, and the
    /// notice falls away with the discarded opt-in.
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

    /// With `RUST_LOG` unset the entrypoint's defaults apply unchanged: the `-d`
    /// arm reaches mtui's DEBUG and still caps the transport.
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

    /// Run `body` with `$RUST_LOG` set (or removed), restoring it afterwards.
    /// Callers must hold `#[serial(env)]`, this crate's **only** exclusion domain
    /// for the process-global environment — `edit`'s `$EDITOR` spawns and
    /// `notification`'s `var_os` probe are on it too, and a second private lock
    /// elsewhere would falsify the SAFETY claim below without failing a test.
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial(env)]` on
    // every caller makes the mutation exclusive.
    #[allow(unsafe_code)]
    fn with_rust_log<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let previous = std::env::var("RUST_LOG").ok();
        // SAFETY: serialised via `#[serial(env)]`, held by every env-touching
        // test in this crate, so no other test observes, mutates or inherits the
        // environment concurrently.
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

    /// Emit the four probe events under `filter` on a **scoped** subscriber (the
    /// process-global default installs only once) and return what reached the
    /// writer. The transport line is hyper-util 0.1.20's leak shape
    /// (`pool.rs:401`) verbatim, and `s3cret` is unique to it.
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

    /// A `MakeWriter` over a shared buffer, so a scoped subscriber's output is
    /// inspectable without touching the process-global default.
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

    /// Reloading through the handle changes which events pass, exactly as the
    /// sink `init_tracing` installs does. Mirrors the reload wiring on a scoped
    /// subscriber so it does not touch the process-global default.
    #[test]
    fn reload_handle_changes_active_level_at_runtime() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (filter, handle) =
            tracing_subscriber::reload::Layer::new(EnvFilter::new(level_directive(LogLevel::Info)));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(BufMaker(Arc::clone(&buf))),
        );

        // Same shape as the closure `init_tracing` returns.
        let mut sink: LogLevelSink = Box::new(move |level: LogLevel| {
            let _ = handle.reload(EnvFilter::new(level_directive(level)));
        });

        with_default(subscriber, || {
            tracing::debug!("hidden at info");
            tracing::info!("visible at info");
            sink(LogLevel::Debug);
            tracing::debug!("visible at debug");
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
