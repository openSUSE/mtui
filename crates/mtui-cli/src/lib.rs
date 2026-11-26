//! `mtui-cli` — the interactive REPL library behind the `mtui` binary.
//!
//! [`run`] is a thin shell: it parses the top-level args, builds the
//! [`Session`] and command [`Registry`](mtui_core::Registry), and drives
//! [`Repl::run`]. Exposing the REPL as a library lets `tests/**` exercise the
//! loop's `repl::step` seam without a TTY.

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
pub(crate) mod tui_edit;

pub use notification::notify_user;
pub use prompt::MtuiPrompt;
pub use repl::{Repl, ReplExit};
pub use startup::seed_session;

use std::io::Write;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use clap::Parser;
use mtui_core::{
    Args, ColorMode, LogLevel, LogLevelSink, Session, level_directive, register_all,
    resolve_log_directives,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The `mtui` REPL entry point.
///
/// Parses the top-level [`Args`] (clap handles `--help`/`--version` and usage errors,
/// exiting itself), initialises `tracing` from `-d/--debug` + `RUST_LOG`, seeds the
/// session from `-a`/`-k` and `--sut` ([`seed_session`]), then enters the REPL.
///
/// The REPL is this binary's **only** driving surface (headless single-command
/// dispatch is an `mtui-mcp`/embedding concern). Config resolves through
/// [`Args::resolve_config`]: the file layers via
/// [`Config::load`](mtui_config::Config::load), then the CLI overrides on top.
pub fn run() -> anyhow::Result<()> {
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

    // `Session::new` defaults the display to `ColorMode::Never`; without this the color
    // helpers never emit ANSI in the live REPL (`Auto` re-checks TTY / `NO_COLOR` /
    // `COLOR` per call).
    session.display.set_color(color);

    // Composition root: the REPL's sinks. `mtui-mcp` installs only its own log
    // level control, so there the toasts and the timeout prompt are no-ops.
    // The notify backend is itself a no-op off a TTY, so a piped REPL is safe.
    session.set_notify_sink(Box::new(|msg: &str, error: bool| {
        notify_user(msg, error);
    }));

    session.set_log_level_sink(log_level_sink);

    // Installed before `seed_session`, so hosts connected during `-a` seeding
    // already carry the SSH command-timeout question ("keep waiting? [Y/n]").
    let terminating = Arc::new(std::sync::atomic::AtomicBool::new(false));
    session.set_prompter(repl::terminal_prompter(Arc::clone(&terminating)));

    // A failed explicit update exits here rather than entering an empty REPL.
    if let ControlFlow::Break(code) = runtime.block_on(seed_session(&registry, &mut session, &args))
    {
        std::process::exit(code.into());
    }

    // Shared so the reedline-owned tab completer reads the same live session the
    // loop dispatches against (see `Repl`).
    let session = Arc::new(Mutex::new(session));
    let mut repl = Repl::new(registry, session, terminating);

    let ending = runtime.block_on(repl.run())?;
    let Some(status) = ending.status() else {
        return Ok(());
    };
    // Force-quit or a teardown on SIGHUP/SIGTERM: drop exactly one destructor
    // (`into_line_editor`, whose `FileBackedHistory` persists on drop) and leave.
    // //
    // `process::exit`, not `main() -> ExitCode`: returning drops the runtime, which
    // blocks on in-flight `spawn_blocking`, e.g. the stdin prompter whose read never
    // returns.
    drop(repl.into_line_editor());
    std::process::exit(status.into());
}

/// A spinner-aware stderr writer for the `tracing` subscriber: each record is written
/// under a [`mtui_hosts::suspend`] guard so a live TTY spinner erases its frame first
/// and the record lands on a clean line. With no spinner (notably off a TTY) it is a
/// plain stderr writer.
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
/// **`DEBUG` raises mtui's targets only, by whichever knob turned it on.** `-d` and a
/// runtime `set_log_level debug` go through [`level_directive`], which appends
/// [`TRANSPORT_LOG_CARVE_OUT`](mtui_core::TRANSPORT_LOG_CARVE_OUT);
/// [`resolve_log_directives`] applies the same cap *on top* of a `RUST_LOG`, so
/// `RUST_LOG=debug` cannot switch the HTTP transport's `DEBUG` back on: those crates
/// log a pool authority that can carry redirect-supplied userinfo. Naming a transport
/// target (`RUST_LOG=hyper_util=debug`) is an informed opt-in, honoured verbatim and
/// announced on stderr at startup.
///
/// At the **default** level the output is a lowercased colored level token, `": "` and
/// the message ([`logfmt::CompactLevelFormat`]), with escapes resolved from `color`
/// through the same [`ColorMode::resolve`] the display uses; `-d/--debug` keeps the
/// verbose Rust format. The user-facing *command error* is rendered by the session
/// display (`repl::render_error`), not this subscriber, so a failing command never
/// prints twice.
///
/// **Runtime reload.** The `EnvFilter` sits behind a [`tracing_subscriber::reload`]
/// layer and the returned [`LogLevelSink`] flips it, backing `set_log_level` (install
/// it with [`set_log_level_sink`](mtui_core::Session::set_log_level_sink)); the
/// [`Handle`](tracing_subscriber::reload::Handle) stays inside the closure to keep
/// `tracing_subscriber` out of the lower crates. A reload **replaces the whole
/// filter**, discarding the startup per-target `RUST_LOG` directives (an explicit
/// transport opt-in included), and changes the level filter only, never the event
/// format (consistent with [`logfmt`]).
#[must_use = "install the sink via Session::set_log_level_sink, or `set_log_level` has no effect"]
pub fn init_tracing(debug: bool, color: ColorMode) -> LogLevelSink {
    let (filter, notice) = startup_filter(debug);
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let registry = tracing_subscriber::registry().with(filter);
    if debug {
        // Stock verbose format, but still spinner-aware so a mid-fan-out DEBUG
        // line erases the live frame before printing.
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(SpinnerAwareStderr))
            .init();
    } else {
        // The subscriber's own ANSI is off so only the custom format's level coloring
        // emits escapes, sharing the `ColorMode::resolve` decision with the display.
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

/// The startup `EnvFilter` and the optional one-line stderr notice, resolved from
/// `$RUST_LOG` and this process's own defaults.
///
/// A seam, so the composition is testable without a global subscriber. Defaults come
/// from [`level_directive`] and `RUST_LOG` is layered on by
/// [`resolve_log_directives`], the helper `mtui-mcp` also uses, so the entrypoints
/// cannot diverge.
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

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
