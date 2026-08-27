//! reedline [`Completer`] adapter over the command surface.
//!
//! An **adapter**, not new completion logic: it translates reedline's
//! `(line, pos)` into the `(text, line)` the registry commands expect,
//! dispatches to [`Command::complete`](mtui_core::Command::complete) (or
//! [`Registry::keys`] — names **and** aliases — for the first token), and
//! re-emits [`Suggestion`]s.
//!
//! [`reedline::Completer::complete`] receives no session, but a command's
//! `complete(session, …)` needs one (loaded RRIDs, host names and templates
//! drive the candidates), so the completer holds a clone of the same
//! `Arc<Mutex<Session>>` the [`Repl`](crate::repl::Repl) loop drives. Completion
//! runs *during* `read_line` and dispatch *after* it returns, so its short-lived
//! lock never overlaps the per-line dispatch lock.

use std::sync::{Arc, Mutex};

use mtui_core::{Registry, Session};
use reedline::{Completer, CompletionResult, Span, Suggestion};

/// reedline completer that defers first-token completion to the [`Registry`] and
/// argument completion to each command's `complete()`.
pub struct MtuiCompleter {
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
}

impl MtuiCompleter {
    /// Builds a completer sharing `registry` and `session` with the REPL loop.
    #[must_use]
    pub(crate) fn new(registry: Arc<Registry>, session: Arc<Mutex<Session>>) -> Self {
        Self { registry, session }
    }
}

/// Splits `line` into `(word_before_cursor, begidx)`.
///
/// `text` is the contiguous non-whitespace tail of `line`, `begidx` the byte
/// offset where it starts (matching reedline's [`Span`] contract). A `line`
/// ending in whitespace gives an empty `text` at `line.len()`, and the command
/// completer is still invoked.
fn split_text_word(line: &str) -> (&str, usize) {
    if line.is_empty() {
        return ("", 0);
    }
    // `+ 1` maps just past the separator; space and tab are both 1 byte.
    let last_ws = line.rfind([' ', '\t']).map_or(0, |i| i + 1);
    (&line[last_ws..], last_ws)
}

impl Completer for MtuiCompleter {
    /// Returns completion candidates for the buffer `line` up to byte offset
    /// `pos`.
    ///
    /// A first token (`begidx == 0`) completes over registry names **and
    /// aliases** by case-sensitive prefix; otherwise the first token names the
    /// command whose `complete(session, text, line)` is delegated to, and an
    /// unknown token or a command with no completer yields nothing.
    ///
    /// A poisoned session lock is recovered ([`into_inner`](std::sync::PoisonError::into_inner))
    /// rather than panicking — a bad completion must never tear down the REPL.
    /// Candidates are computed synchronously, so the answer is always
    /// [`CompletionResult::Fresh`].
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let before = &line[..pos.min(line.len())];
        // Left-trimmed before computing offsets; the shift is tracked so the
        // reported span still indexes the *original* buffer in bytes.
        let leading = before.len() - before.trim_start().len();
        let stripped = &before[leading..];

        let (text, begidx_in_stripped) = split_text_word(stripped);
        let span = Span::new(leading + begidx_in_stripped, pos.min(line.len()));

        let candidates = if begidx_in_stripped == 0 {
            self.registry
                .keys()
                .filter(|key| key.starts_with(text))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else {
            let first_token = stripped.split(' ').next().unwrap_or("");
            // The trait's `complete(session, …)` has no registry handle, so the
            // adapter supplies `help <cmd>`'s candidates itself.
            if first_token == "help" {
                self.registry
                    .keys()
                    .filter(|key| key.starts_with(text))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            } else {
                match self.registry.get(first_token) {
                    None => Vec::new(),
                    Some(cmd) => {
                        let session = self
                            .session
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        cmd.complete(&session, text, stripped)
                    }
                }
            }
        };

        CompletionResult::fresh(
            candidates
                .into_iter()
                .map(|value| Suggestion {
                    value,
                    span,
                    append_whitespace: true,
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )
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

    /// A command whose `complete()` returns a fixed candidate list.
    struct FixedCmd;

    #[async_trait]
    impl Command for FixedCmd {
        fn name(&self) -> &'static str {
            "run"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["r"]
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
            ["--host", "--all-templates", "reboot"]
                .into_iter()
                .filter(|c| c.starts_with(text))
                .map(str::to_owned)
                .collect()
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// A command whose `complete()` reads live session state, proving the
    /// `Arc<Mutex<Session>>` bridge exposes the live session rather than a
    /// snapshot. It reads a trivially-public field so no host fixtures are needed.
    struct SessionProbeCmd;

    #[async_trait]
    impl Command for SessionProbeCmd {
        fn name(&self) -> &'static str {
            "shell"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
            let candidate = if session.is_repl {
                "interactive"
            } else {
                "headless"
            };
            [candidate]
                .into_iter()
                .filter(|c| c.starts_with(text))
                .map(str::to_owned)
                .collect()
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// A command with no `complete()` override (default empty).
    struct BareCmd;

    #[async_trait]
    impl Command for BareCmd {
        fn name(&self) -> &'static str {
            "reboot"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    fn completer() -> MtuiCompleter {
        let mut registry = Registry::new();
        registry.register(Arc::new(FixedCmd));
        registry.register(Arc::new(SessionProbeCmd));
        registry.register(Arc::new(BareCmd));
        let session = Session::new(Config::default(), true);
        MtuiCompleter::new(Arc::new(registry), Arc::new(Mutex::new(session)))
    }

    fn values(result: &CompletionResult) -> Vec<&str> {
        result
            .suggestions()
            .iter()
            .map(|s| s.value.as_str())
            .collect()
    }

    // ---- split_text_word --------------------------------------------------

    #[test]
    fn split_empty_line() {
        assert_eq!(split_text_word(""), ("", 0));
    }

    #[test]
    fn split_single_word() {
        assert_eq!(split_text_word("run"), ("run", 0));
    }

    #[test]
    fn split_trailing_space_yields_empty_tail_at_end() {
        assert_eq!(split_text_word("run "), ("", 4));
    }

    #[test]
    fn split_partial_second_word() {
        assert_eq!(split_text_word("run --h"), ("--h", 4));
    }

    #[test]
    fn split_tab_separator() {
        assert_eq!(split_text_word("run\t--h"), ("--h", 4));
    }

    // ---- first-token completion -------------------------------------------

    #[test]
    fn first_token_empty_offers_all_names_and_aliases() {
        let mut c = completer();
        let s = c.complete("", 0);
        // Insertion order, each command's name before its own aliases.
        assert_eq!(values(&s), vec!["run", "r", "shell", "reboot"]);
    }

    #[test]
    fn first_token_completion_is_never_provisional() {
        // A `Pending`/`Stale` answer would silently empty the Tab menu.
        let mut c = completer();
        let s = c.complete("r", 1);
        assert!(!s.is_provisional());
    }

    #[test]
    fn first_token_prefix_filters() {
        let mut c = completer();
        let s = c.complete("r", 1);
        let mut got = values(&s);
        got.sort_unstable();
        assert_eq!(got, vec!["r", "reboot", "run"]);
    }

    #[test]
    fn first_token_completes_aliases() {
        let mut c = completer();
        let s = c.complete("r", 1);
        assert!(
            values(&s).contains(&"r"),
            "alias `r` must be a first-token candidate"
        );
    }

    #[test]
    fn first_token_is_case_sensitive() {
        let mut c = completer();
        assert!(c.complete("R", 1).suggestions().is_empty());
    }

    #[test]
    fn first_token_span_covers_whole_token() {
        let mut c = completer();
        let s = c.complete("ru", 2);
        assert_eq!(s.suggestions()[0].span, Span::new(0, 2));
    }

    // ---- per-command completion -------------------------------------------

    #[test]
    fn known_command_delegates_to_its_completer() {
        let mut c = completer();
        let s = c.complete("run --", 6);
        let mut got = values(&s);
        got.sort_unstable();
        assert_eq!(got, vec!["--all-templates", "--host"]);
    }

    #[test]
    fn known_command_with_partial_arg_filters() {
        let mut c = completer();
        let s = c.complete("run --h", 7);
        assert_eq!(values(&s), vec!["--host"]);
        // The span replaces just the partial arg, not the command.
        assert_eq!(s.suggestions()[0].span, Span::new(4, 7));
    }

    #[test]
    fn unknown_first_token_yields_nothing() {
        let mut c = completer();
        assert!(c.complete("nope --x", 8).suggestions().is_empty());
    }

    #[test]
    fn command_without_completer_yields_nothing() {
        let mut c = completer();
        assert!(c.complete("reboot ", 7).suggestions().is_empty());
    }

    /// A bare command named `help`, so the adapter's registry-backed
    /// `help`-argument special case is reachable.
    struct HelpCmd;

    #[async_trait]
    impl Command for HelpCmd {
        fn name(&self) -> &'static str {
            "help"
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    #[test]
    fn help_argument_completes_over_command_names() {
        let mut registry = Registry::new();
        registry.register(Arc::new(FixedCmd)); // name "run", alias "r"
        registry.register(Arc::new(HelpCmd));
        let session = Session::new(Config::default(), true);
        let mut c = MtuiCompleter::new(Arc::new(registry), Arc::new(Mutex::new(session)));

        let s = c.complete("help r", 6);
        let mut got = values(&s);
        got.sort_unstable();
        // An empty tail offers every command key.
        assert_eq!(got, vec!["r", "run"]);
        let s = c.complete("help ", 5);
        let all = values(&s);
        assert!(all.contains(&"run") && all.contains(&"help"));
    }

    // ---- session-aware completion (the Arc<Mutex<Session>> bridge) ---------

    #[test]
    fn command_reads_live_session_state() {
        let mut registry = Registry::new();
        registry.register(Arc::new(SessionProbeCmd));
        let session = Session::new(Config::default(), true);
        let mut c = MtuiCompleter::new(Arc::new(registry), Arc::new(Mutex::new(session)));
        assert_eq!(values(&c.complete("shell ", 6)), vec!["interactive"]);
    }

    #[test]
    fn completer_reflects_headless_session() {
        // The mirror case: a headless session flips the probe's answer, proving
        // the completer reads the live session, not a baked-in snapshot.
        let mut registry = Registry::new();
        registry.register(Arc::new(SessionProbeCmd));
        let session = Session::new(Config::default(), false);
        let mut c = MtuiCompleter::new(Arc::new(registry), Arc::new(Mutex::new(session)));
        assert_eq!(values(&c.complete("shell ", 6)), vec!["headless"]);
    }

    // ---- span byte-correctness with leading whitespace --------------------

    #[test]
    fn leading_whitespace_span_indexes_original_buffer() {
        let mut c = completer();
        // Two leading spaces, so the "r" partial sits at bytes 6..7 of the
        // *original* buffer, which is what the span must index.
        let line = "  run r";
        let s = c.complete(line, line.len());
        assert_eq!(values(&s), vec!["reboot"]);
        assert_eq!(s.suggestions()[0].span, Span::new(6, 7));
        assert_eq!(
            &line[s.suggestions()[0].span.start..s.suggestions()[0].span.end],
            "r"
        );
    }
}
