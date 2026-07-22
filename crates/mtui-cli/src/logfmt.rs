//! Custom compact `tracing` event format mirroring upstream's `ColorFormatter`.
//!
//! Upstream mtui routes every log record through a single
//! `ColorFormatter("%(levelname)s: %(message)s")`
//! (`python-mtui/mtui/cli/colors/formatter.py`): a **lowercased**, colorized
//! level token (green `info` / yellow `warning` / red `error`), then `": "`, then
//! the message — no timestamp, no module path. mtui already renders *command
//! errors* that way through the session display (see `repl::render_error`); this
//! layer brings `tracing::info!`/`warn!` (and any other event at the default
//! verbosity) to the same look so the two channels are consistent
//! (`mtui-rs-ilt`, follow-up to `mtui-rs-7h9`).
//!
//! **ANSI decision is shared with the display.** Whether escapes are emitted is
//! computed once from the resolved [`ColorMode`](mtui_core::ColorMode) handed to
//! [`init_tracing`](crate::init_tracing) — the same `ColorMode::resolve()` the
//! display uses — so `--color auto/always/never` governs the level token
//! identically to the `error:` line. The subscriber's own ANSI is disabled
//! (`with_ansi(false)`) so only this layer's explicit coloring emits escapes.
//!
//! **Deviation from upstream:** the DEBUG-only `" [module:function]"` suffix is
//! intentionally not reproduced (low value in Rust — under `-d/--debug` the
//! verbose Rust format restores the module `target`, which covers the diagnostic
//! need). This compact layer is only installed at the *default* verbosity;
//! `-d/--debug` keeps the stock verbose format (timestamp + level + target).

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
/// Construct via [`CompactLevelFormat::new`], passing whether ANSI escapes
/// should be emitted (already resolved from the process `ColorMode`).
#[derive(Debug, Clone, Copy)]
pub struct CompactLevelFormat {
    ansi: bool,
}

impl CompactLevelFormat {
    /// Builds the format with the ANSI decision already resolved.
    #[must_use]
    pub const fn new(ansi: bool) -> Self {
        Self { ansi }
    }

    /// The lowercased level token, colorized per upstream `COLORS` when ANSI is
    /// on: info→green, warn→yellow, error→red. `trace`/`debug` fall through
    /// uncolored (they only appear under `-d`, which uses the verbose format
    /// anyway, but stay defensive).
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
        // The field formatter writes the `message` field (and any structured
        // fields) exactly as the stock compact format does — but without the
        // level/timestamp/target prefix we deliberately omit.
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
        // No module target and no ISO-8601 timestamp leak into the compact line.
        assert!(!out.contains("mtui_cli"), "no target: {out:?}");
        assert!(!out.contains("logfmt"), "no target: {out:?}");
        assert!(
            !out.contains('T') || !out.contains('Z'),
            "no RFC3339: {out:?}"
        );
        // Plain (ansi=false): no escapes at all.
        assert!(!out.contains('\u{1b}'), "no escapes when off: {out:?}");
        // Each event is exactly one line.
        assert_eq!(out.lines().count(), 3, "one line per event: {out:?}");
    }

    #[test]
    fn full_format_colorizes_level_token_when_ansi_on() {
        let out = render_via_layer(true);
        assert!(out.contains('\u{1b}'), "escapes present: {out:?}");
        // Lowercased tokens still present inside the colored spans; message text
        // stays uncolored (only the level token is wrapped).
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
    fn level_token_colored_matches_upstream_palette() {
        let f = CompactLevelFormat::new(true);
        let info = f.level_token(&Level::INFO);
        let warn = f.level_token(&Level::WARN);
        let error = f.level_token(&Level::ERROR);
        // Escapes present and the lowercased name survives inside them.
        for (tok, name) in [(&info, "info"), (&warn, "warn"), (&error, "error")] {
            assert!(tok.contains('\u{1b}'), "escape present: {tok:?}");
            assert!(tok.contains(name), "name present: {tok:?}");
        }
        // Distinct colors per level (green/yellow/red differ byte-wise).
        assert_ne!(info, warn);
        assert_ne!(warn, error);
        assert_ne!(info, error);
        // Parity with the display's `error` token (both via owo-colors red).
        assert_eq!(error, "error".red().to_string());
    }

    /// An event marked with [`CLAP_PREFIXED_TARGET`] renders its message
    /// verbatim, with no `"{level}: "` prefix added on top — the mechanism
    /// that lets a genuine clap usage error (which already carries its own
    /// `"error: "` prefix) avoid being double-prefixed.
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
