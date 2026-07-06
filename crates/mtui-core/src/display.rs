//! Formatted command output (`CommandPromptDisplay`).
//!
//! Port of upstream `mtui.cli.display.CommandPromptDisplay`, scoped to the
//! surface the Phase-5 command engine needs today: [`println`](CommandPromptDisplay::println)
//! and [`template_banner`](CommandPromptDisplay::template_banner) (the fan-out
//! label), plus a [`ColorMode`] toggle gating the `green`/`red`/`yellow`
//! helpers. The wider `list_*` family (host status, locks, bugs, versions, …)
//! lands with its own task (P5.3) once the commands that call it exist.
//!
//! Output is captured through a boxed [`std::io::Write`] sink so tests can
//! snapshot it and the REPL/MCP can point it at stdout or a buffer.

use std::io::Write;

use owo_colors::OwoColorize;

/// Whether ANSI color escapes are emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Always emit color escapes.
    Always,
    /// Never emit color escapes (plain text). The default for non-TTY sinks
    /// (buffers, MCP, redirected stdout).
    #[default]
    Never,
}

/// Handles the display of formatted output in the command prompt.
///
/// Owns its output sink; construct with [`with_sink`](Self::with_sink) for tests
/// (a `Vec<u8>` buffer) or [`stdout`](Self::stdout) for the interactive REPL.
pub struct CommandPromptDisplay {
    output: Box<dyn Write + Send>,
    color: ColorMode,
}

impl CommandPromptDisplay {
    /// Builds a display over an arbitrary sink with an explicit color mode.
    #[must_use]
    pub fn with_sink(output: Box<dyn Write + Send>, color: ColorMode) -> Self {
        Self { output, color }
    }

    /// Builds a display writing to stdout.
    ///
    /// Color defaults to [`ColorMode::Never`]; the REPL flips it to
    /// [`ColorMode::Always`] when attached to a TTY (Phase 6).
    #[must_use]
    pub fn stdout() -> Self {
        Self {
            output: Box::new(std::io::stdout()),
            color: ColorMode::Never,
        }
    }

    /// The active color mode.
    #[must_use]
    pub const fn color(&self) -> ColorMode {
        self.color
    }

    /// Sets the color mode (e.g. the REPL enabling color on a TTY).
    pub fn set_color(&mut self, color: ColorMode) {
        self.color = color;
    }

    /// Writes `msg` followed by `eol` to the output sink.
    ///
    /// Mirrors upstream `println(msg, eol="\n")`. Write errors are swallowed to
    /// match the Python surface, which never surfaces stdout write failures from
    /// display helpers.
    pub fn println(&mut self, msg: &str) {
        let _ = writeln!(self.output, "{msg}");
    }

    /// Writes `msg` followed by `eol` with an explicit end-of-line string.
    pub fn print_eol(&mut self, msg: &str, eol: &str) {
        let _ = write!(self.output, "{msg}{eol}");
    }

    /// Prints a per-template banner used to label fan-out output.
    ///
    /// Printed before each template's output block when a command fans out
    /// across more than one loaded template, so the user can tell which template
    /// produced which result. Upstream renders exactly `=== {rrid} ===`.
    pub fn template_banner(&mut self, rrid: &str) {
        self.println(&format!("=== {rrid} ==="));
    }

    /// Wraps `text` in green when color is enabled, else returns it unchanged.
    #[must_use]
    pub fn green(&self, text: &str) -> String {
        match self.color {
            ColorMode::Always => text.green().to_string(),
            ColorMode::Never => text.to_owned(),
        }
    }

    /// Wraps `text` in red when color is enabled, else returns it unchanged.
    #[must_use]
    pub fn red(&self, text: &str) -> String {
        match self.color {
            ColorMode::Always => text.red().to_string(),
            ColorMode::Never => text.to_owned(),
        }
    }

    /// Wraps `text` in yellow when color is enabled, else returns it unchanged.
    #[must_use]
    pub fn yellow(&self, text: &str) -> String {
        match self.color {
            ColorMode::Always => text.yellow().to_string(),
            ColorMode::Never => text.to_owned(),
        }
    }
}

impl Default for CommandPromptDisplay {
    fn default() -> Self {
        Self::stdout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A display over a shared buffer, returning the handle to inspect output.
    fn buffered(
        color: ColorMode,
    ) -> (
        CommandPromptDisplay,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = SharedSink(buf.clone());
        (CommandPromptDisplay::with_sink(Box::new(sink), color), buf)
    }

    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rendered(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn template_banner_matches_upstream() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.template_banner("SUSE:Maintenance:1:1");
        assert_eq!(rendered(&buf), "=== SUSE:Maintenance:1:1 ===\n");
    }

    #[test]
    fn println_appends_newline() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.println("hello");
        assert_eq!(rendered(&buf), "hello\n");
    }

    #[test]
    fn color_never_emits_no_escapes() {
        let d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);
        assert_eq!(d.green("ok"), "ok");
        assert!(!d.red("bad").contains('\u{1b}'));
    }

    #[test]
    fn color_always_emits_escapes() {
        let d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        assert!(d.green("ok").contains('\u{1b}'));
        assert!(d.red("bad").contains('\u{1b}'));
        assert!(d.yellow("warn").contains('\u{1b}'));
    }

    #[test]
    fn color_accessor_and_setter_roundtrip() {
        let mut d = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Never);
        assert_eq!(d.color(), ColorMode::Never);
        d.set_color(ColorMode::Always);
        assert_eq!(d.color(), ColorMode::Always);
    }

    #[test]
    fn print_eol_uses_explicit_terminator() {
        let (mut d, buf) = buffered(ColorMode::Never);
        d.print_eol("a", "");
        d.print_eol("b", "|");
        assert_eq!(rendered(&buf), "ab|");
    }

    #[test]
    fn default_and_stdout_construct_without_panic() {
        let _ = CommandPromptDisplay::default();
        let d = CommandPromptDisplay::stdout();
        assert_eq!(d.color(), ColorMode::Never);
    }

    #[test]
    fn color_mode_default_is_never() {
        assert_eq!(ColorMode::default(), ColorMode::Never);
    }
}
