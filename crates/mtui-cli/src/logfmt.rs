//! Custom compact `tracing` event format for the REPL.
//!
//! One line per record: a **lowercased**, colorized level token, `": "`, then
//! the message — no timestamp, no module path. That is how the session display
//! already renders command errors (`repl::render_error`), so bringing
//! `tracing::info!`/`warn!` to the same look makes the two channels
//! indistinguishable to an operator reading the terminal.
//!
//! **The ANSI decision is shared with the display**: it is computed once from
//! the resolved [`ColorMode`](mtui_core::ColorMode) handed to
//! [`init_tracing`](crate::init_tracing), so `--color` governs the level token
//! and the `error:` line identically. The subscriber's own ANSI is disabled so
//! only this layer's explicit coloring emits escapes.
//!
//! Installed at the *default* verbosity only; `-d/--debug` keeps the stock
//! verbose format. mtui deliberately has no DEBUG-only `" [module:function]"`
//! suffix — the verbose format's `target` covers the same need.

use std::fmt;

use owo_colors::OwoColorize;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Marks an event whose message is already fully rendered (e.g. clap's own
/// colored `error: ...` usage text for a genuine parse error) so
/// [`CompactLevelFormat::format_event`] must not prepend a second level
/// prefix. Set via `tracing::error!(target: CLAP_PREFIXED_TARGET, "{msg}")`.
pub(crate) const CLAP_PREFIXED_TARGET: &str = "mtui::clap_prefixed";

/// A [`FormatEvent`] that renders `"{level}: {message}"` with a lowercased,
/// optionally colorized level token and no timestamp/target.
///
/// Construct via `CompactLevelFormat::new`, passing whether ANSI escapes
/// should be emitted (already resolved from the process `ColorMode`).
#[derive(Debug, Clone, Copy)]
pub struct CompactLevelFormat {
    ansi: bool,
}

impl CompactLevelFormat {
    /// Builds the format with the ANSI decision already resolved.
    #[must_use]
    pub(crate) const fn new(ansi: bool) -> Self {
        Self { ansi }
    }

    /// The lowercased level token, colorized when ANSI is on: info→green,
    /// warn→yellow, error→red. `trace`/`debug` stay uncolored — they only appear
    /// under `-d`, which uses the verbose format anyway.
    fn level_token(self, level: &Level) -> String {
        let name = match *level {
            Level::ERROR => "error",
            Level::WARN => "warn",
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        };
        if !self.ansi {
            return name.to_owned();
        }
        match *level {
            Level::ERROR => name.red().to_string(),
            Level::WARN => name.yellow().to_string(),
            Level::INFO => name.green().to_string(),
            Level::DEBUG | Level::TRACE => name.to_owned(),
        }
    }
}

impl<S, N> FormatEvent<S, N> for CompactLevelFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        if meta.target() != CLAP_PREFIXED_TARGET {
            write!(writer, "{}: ", self.level_token(meta.level()))?;
        }
        // The stock field rendering, minus the level/timestamp/target prefix.
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

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

    /// Renders `info!`/`warn!`/`error!` events through the real
    /// [`CompactLevelFormat`] layer into a buffer and returns the captured text.
    fn render_via_layer(ansi: bool) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(CompactLevelFormat::new(ansi))
            .with_writer(BufMaker(Arc::clone(&buf)))
            .finish();
        with_default(subscriber, || {
            tracing::info!("hello info");
            tracing::warn!("hello warn");
            tracing::error!("hello error");
        });
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn full_format_is_level_message_without_timestamp_or_target() {
        let out = render_via_layer(false);
        assert!(out.contains("info: hello info"), "got: {out:?}");
        assert!(out.contains("warn: hello warn"), "got: {out:?}");
        assert!(out.contains("error: hello error"), "got: {out:?}");
        assert!(!out.contains("mtui_cli"), "no target: {out:?}");
        assert!(!out.contains("logfmt"), "no target: {out:?}");
        assert!(
            !out.contains('T') || !out.contains('Z'),
            "no RFC3339: {out:?}"
        );
        assert!(!out.contains('\u{1b}'), "no escapes when off: {out:?}");
        assert_eq!(out.lines().count(), 3, "one line per event: {out:?}");
    }

    #[test]
    fn full_format_colorizes_level_token_when_ansi_on() {
        let out = render_via_layer(true);
        assert!(out.contains('\u{1b}'), "escapes present: {out:?}");
        // Only the level token is wrapped; the message text stays uncolored.
        assert!(out.contains("hello info"), "message present: {out:?}");
        assert!(out.contains("hello error"), "message present: {out:?}");
    }

    #[test]
    fn level_token_plain_is_lowercased_no_escapes() {
        let f = CompactLevelFormat::new(false);
        assert_eq!(f.level_token(&Level::INFO), "info");
        assert_eq!(f.level_token(&Level::WARN), "warn");
        assert_eq!(f.level_token(&Level::ERROR), "error");
        for l in [Level::INFO, Level::WARN, Level::ERROR] {
            assert!(!f.level_token(&l).contains('\u{1b}'), "no escapes when off");
        }
    }

    #[test]
    fn level_token_colored_with_stable_palette() {
        let f = CompactLevelFormat::new(true);
        let info = f.level_token(&Level::INFO);
        let warn = f.level_token(&Level::WARN);
        let error = f.level_token(&Level::ERROR);
        for (tok, name) in [(&info, "info"), (&warn, "warn"), (&error, "error")] {
            assert!(tok.contains('\u{1b}'), "escape present: {tok:?}");
            assert!(tok.contains(name), "name present: {tok:?}");
        }
        assert_ne!(info, warn);
        assert_ne!(warn, error);
        assert_ne!(info, error);
        // Parity with the display's `error` token (both owo-colors red).
        assert_eq!(error, "error".red().to_string());
    }

    /// An event marked with [`CLAP_PREFIXED_TARGET`] renders verbatim, which is
    /// what keeps a clap usage error from being double-prefixed.
    #[test]
    fn clap_prefixed_target_suppresses_level_prefix() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .event_format(CompactLevelFormat::new(false))
            .with_writer(BufMaker(Arc::clone(&buf)))
            .finish();
        with_default(subscriber, || {
            tracing::error!(target: CLAP_PREFIXED_TARGET, "error: already prefixed");
        });
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(out, "error: already prefixed\n");
    }
}
