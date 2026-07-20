//! reedline [`Highlighter`] over the REPL input line.
//!
//! Ports upstream `mtui/cli/_lexer.py::MtuiCommandLexer`, giving a quick visual
//! signal about what the user has typed *before* pressing Enter:
//!
//! * The **first token** (the command name) is green when it names a registered
//!   command ([`Registry::contains`], which matches command names *and* aliases)
//!   and red otherwise — making typos / out-of-context commands visible.
//! * Later tokens starting with `-` / `--` (flags) are cyan.
//! * Everything else (positional args, values) uses the terminal default.
//!
//! Whitespace runs are preserved verbatim so the styled text round-trips the
//! input exactly (reedline re-renders the line from the `(Style, String)`
//! chunks; dropping a character would shift the cursor — same requirement as
//! upstream `_tokenize`).
//!
//! Coloring is gated on the live [`ColorMode`](mtui_core::ColorMode): when it
//! resolves to *off* (`--color never`, or `auto` piped to a non-TTY), every
//! chunk is emitted unstyled so scraped / piped output stays plain.
//!
//! Like [`MtuiPrompt`](crate::prompt::MtuiPrompt) and
//! [`MtuiCompleter`](crate::completer::MtuiCompleter), this holds the shared
//! `Arc<Registry>` / `Arc<Mutex<Session>>` and reads them synchronously during
//! reedline's redraw — never overlapping the post-`read_line` dispatch lock.

use std::sync::{Arc, Mutex};

use mtui_core::{Registry, Session};
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

/// A whitespace-vs-token chunk of an input line (mirrors upstream `_tokenize`).
enum Chunk<'a> {
    /// A run of spaces / tabs, preserved verbatim.
    Ws(&'a str),
    /// A run of non-whitespace characters (a token).
    Tok(&'a str),
}

/// Splits `line` into alternating whitespace / non-whitespace chunks, preserving
/// every character so the styled output round-trips the input.
fn tokenize(line: &str) -> Vec<Chunk<'_>> {
    let mut chunks = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let is_ws = rest.starts_with([' ', '\t']);
        let end = rest
            .find(|c: char| (c == ' ' || c == '\t') != is_ws)
            .unwrap_or(rest.len());
        let (chunk, tail) = rest.split_at(end);
        chunks.push(if is_ws {
            Chunk::Ws(chunk)
        } else {
            Chunk::Tok(chunk)
        });
        rest = tail;
    }
    chunks
}

/// Highlights the first token by command-known-ness and flag tokens cyan.
pub struct MtuiHighlighter {
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
}

impl MtuiHighlighter {
    /// Builds a highlighter sharing `registry` and `session` with the REPL loop.
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<Mutex<Session>>) -> Self {
        Self { registry, session }
    }

    /// Whether colored output is currently enabled (live [`ColorMode`] resolve).
    fn color_enabled(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .display
            .color()
            .resolve()
    }
}

impl Highlighter for MtuiHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        let colored = self.color_enabled();
        let default = Style::new();

        let mut saw_command = false;
        for chunk in tokenize(line) {
            match chunk {
                Chunk::Ws(ws) => styled.push((default, ws.to_owned())),
                Chunk::Tok(tok) => {
                    let style = if !colored {
                        default
                    } else if !saw_command {
                        if self.registry.contains(tok) {
                            Color::Green.into()
                        } else {
                            Color::Red.into()
                        }
                    } else if tok.starts_with('-') {
                        Color::Cyan.into()
                    } else {
                        default
                    };
                    styled.push((style, tok.to_owned()));
                    saw_command = true;
                }
            }
        }
        styled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use clap::ArgMatches;
    use mtui_config::Config;
    use mtui_core::command::{Command, Scope};
    use mtui_core::error::CommandResult;
    use mtui_core::{ColorMode, CommandPromptDisplay};

    struct Stub;

    #[async_trait]
    impl Command for Stub {
        fn name(&self) -> &'static str {
            "run"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    fn highlighter(color: ColorMode) -> MtuiHighlighter {
        let mut registry = Registry::new();
        registry.register(Arc::new(Stub));
        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), color);
        let session = Session::with_display(Config::default(), true, display);
        MtuiHighlighter::new(Arc::new(registry), Arc::new(Mutex::new(session)))
    }

    /// Reassembles the styled chunks back into the original line.
    fn reassemble(styled: &StyledText) -> String {
        styled.buffer.iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn known_command_is_green_unknown_is_red() {
        let h = highlighter(ColorMode::Always);
        let known = h.highlight("run", 0);
        assert_eq!(known.buffer[0].0, Color::Green.into());
        let unknown = h.highlight("nope", 0);
        assert_eq!(unknown.buffer[0].0, Color::Red.into());
    }

    #[test]
    fn flags_are_cyan_positionals_default() {
        let h = highlighter(ColorMode::Always);
        let styled = h.highlight("run --host a1 pos", 0);
        // chunks: run, ws, --host, ws, a1, ws, pos
        assert_eq!(styled.buffer[0].0, Color::Green.into()); // run
        assert_eq!(styled.buffer[2].0, Color::Cyan.into()); // --host
        assert_eq!(styled.buffer[4].0, Style::new()); // a1 (positional)
        assert_eq!(styled.buffer[6].0, Style::new()); // pos (positional)
    }

    #[test]
    fn whitespace_is_preserved_verbatim() {
        let h = highlighter(ColorMode::Always);
        let line = "  run   --flag\ttail  ";
        assert_eq!(reassemble(&h.highlight(line, 0)), line);
    }

    #[test]
    fn color_off_emits_only_default_style() {
        let h = highlighter(ColorMode::Never);
        let styled = h.highlight("run --flag nope", 0);
        assert!(
            styled.buffer.iter().all(|(s, _)| *s == Style::new()),
            "every chunk should be unstyled when color is off"
        );
    }

    #[test]
    fn empty_line_yields_no_chunks() {
        let h = highlighter(ColorMode::Always);
        assert!(h.highlight("", 0).buffer.is_empty());
    }
}
