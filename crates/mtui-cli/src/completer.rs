//! reedline [`Completer`] adapter over the Phase-5 command surface.
//!
//! Ports upstream `mtui/cli/_completer.py::MtuiCompleter`, which bridges
//! prompt_toolkit's `Completer` onto the `cmd.Cmd`-style `complete_<name>`
//! methods. Here the bridge is from [`reedline::Completer`] onto the
//! [`Command::complete`](mtui_core::Command::complete) surface every command
//! already implements (P5), plus [`Registry::keys`] (names **and** aliases) for
//! first-token completion.
//!
//! This is an **adapter**, not new completion logic: it translates reedline's
//! `(line, pos)` into the `(text, line)` the registry commands expect,
//! dispatches, and re-emits [`Suggestion`]s.
//!
//! ## Session access
//!
//! [`reedline::Completer::complete`] receives no session, but a command's
//! `complete(session, …)` needs one (loaded RRIDs, host names, templates drive
//! the candidates). The completer therefore holds a clone of the same
//! `Arc<Mutex<Session>>` the [`Repl`](crate::repl::Repl) loop drives and takes a
//! short-lived lock inside [`complete`](MtuiCompleter::complete) to read live
//! state — the analogue of upstream holding a live `CommandPrompt` reference.
//! Completion happens *during* `read_line`; dispatch happens *after* it returns,
//! so the lock never overlaps the per-line dispatch lock.

use std::sync::{Arc, Mutex};

use mtui_core::{Registry, Session};
use reedline::{Completer, Span, Suggestion};

/// reedline completer that defers first-token completion to the [`Registry`] and
/// argument completion to each command's `complete()`.
pub struct MtuiCompleter {
    registry: Arc<Registry>,
    session: Arc<Mutex<Session>>,
}

impl MtuiCompleter {
    /// Builds a completer sharing `registry` and `session` with the REPL loop.
    #[must_use]
    pub fn new(registry: Arc<Registry>, session: Arc<Mutex<Session>>) -> Self {
        Self { registry, session }
    }
}

/// Splits `line` into `(word_before_cursor, begidx)`.
///
/// Mirrors upstream `_split_text_word` / `cmd.Cmd.complete`: `text` is the
/// contiguous non-whitespace tail of `line`, `begidx` the byte offset where that
/// tail starts. When `line` ends in whitespace (e.g. `"run -t "`), `text` is
/// empty and `begidx == line.len()` — the command completer is still invoked.
///
/// Offsets are byte offsets into `line`, matching reedline's [`Span`] contract.
fn split_text_word(line: &str) -> (&str, usize) {
    if line.is_empty() {
        return ("", 0);
    }
    // Last space or tab; `+ len_utf8` maps just past it. Both are 1 byte.
    let last_ws = line.rfind([' ', '\t']).map_or(0, |i| i + 1);
    (&line[last_ws..], last_ws)
}

impl Completer for MtuiCompleter {
    /// Returns completion candidates for the buffer `line` up to byte offset
    /// `pos`.
    ///
    /// Dispatch rules (upstream parity):
    ///
    /// * The buffer before the cursor is `&line[..pos]`, then left-trimmed so
    ///   column offsets match upstream's `lstrip`.
    /// * `begidx == 0` → **first-token** completion: registry command names
    ///   **and aliases** by case-sensitive prefix match (upstream
    ///   `_completer.py` iterates `prompt.commands`, which includes aliases).
    /// * otherwise → **per-command**: look up the first token in the registry
    ///   and delegate to its `complete(session, text, line)`; an unknown token
    ///   or a command with no completer yields nothing.
    ///
    /// A poisoned session lock is recovered ([`into_inner`](std::sync::PoisonError::into_inner))
    /// rather than panicking — a bad completion must never tear down the REPL.
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        // The buffer up to the cursor (reedline hands us the whole line + pos).
        let before = &line[..pos.min(line.len())];
        // Upstream lstrips before computing offsets; track the shift so the
        // reported span still indexes the *original* buffer in bytes.
        let leading = before.len() - before.trim_start().len();
        let stripped = &before[leading..];

        let (text, begidx_in_stripped) = split_text_word(stripped);
        // Span into the original buffer: start past the trimmed leading ws.
        let span = Span::new(leading + begidx_in_stripped, pos.min(line.len()));

        let candidates = if begidx_in_stripped == 0 {
            // First-token completion: registry names *and aliases* by prefix
            // (upstream parity — `prompt.commands` carries aliases as keys).
            self.registry
                .keys()
                .filter(|key| key.starts_with(text))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else {
            // Per-command completion: first token names the command.
            let first_token = stripped.split(' ').next().unwrap_or("");
            // `help <cmd>` completes over command names. Upstream `Help.complete`
            // reaches the registry via the base-class command set; the trait's
            // `complete(session, …)` has no registry handle, so the adapter — which
            // does — supplies the candidates here (canonical names + aliases).
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

        candidates
            .into_iter()
            .map(|value| Suggestion {
                value,
                span,
                append_whitespace: true,
                ..Default::default()
            })
            .collect()
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

    /// A command whose `complete()` reads live session state (`interactive`) —
    /// proves the `Arc<Mutex<Session>>` bridge exposes the *live* session to the
    /// completer rather than a snapshot. Reads a trivially-public field so the
    /// test needs no `mtui-hosts`/`TestReport` fixtures.
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

    fn values(suggestions: &[Suggestion]) -> Vec<&str> {
        suggestions.iter().map(|s| s.value.as_str()).collect()
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
        // "run " → completing a fresh (empty) second token.
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
        // Insertion order, each command's name before its own aliases
        // (`run` carries alias `r`) — upstream `prompt.commands` parity.
        assert_eq!(values(&s), vec!["run", "r", "shell", "reboot"]);
    }

    #[test]
    fn first_token_prefix_filters() {
        let mut c = completer();
        let s = c.complete("r", 1);
        let mut got = values(&s);
        got.sort_unstable();
        // `r` matches the alias `r`, plus `reboot` and `run`.
        assert_eq!(got, vec!["r", "reboot", "run"]);
    }

    #[test]
    fn first_token_completes_aliases() {
        // An alias-only prefix must surface the alias: `run` carries alias `r`,
        // so completing `r` (before any space) offers the alias itself —
        // upstream parity with `prompt.commands` iteration.
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
        assert!(c.complete("R", 1).is_empty());
    }

    #[test]
    fn first_token_span_covers_whole_token() {
        let mut c = completer();
        let s = c.complete("ru", 2);
        assert_eq!(s[0].span, Span::new(0, 2));
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
        // The span replaces just the "--h" partial arg, not the command.
        assert_eq!(s[0].span, Span::new(4, 7));
    }

    #[test]
    fn unknown_first_token_yields_nothing() {
        let mut c = completer();
        assert!(c.complete("nope --x", 8).is_empty());
    }

    #[test]
    fn command_without_completer_yields_nothing() {
        let mut c = completer();
        // `reboot` (BareCmd) has the default empty `complete()`.
        assert!(c.complete("reboot ", 7).is_empty());
    }

    /// A bare command named `help` so the adapter's `help`-argument special case
    /// (registry-backed, since the trait `complete` has no registry) is reachable.
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

        // `help r` → the registry names/aliases starting with "r" (run, r).
        let s = c.complete("help r", 6);
        let mut got = values(&s);
        got.sort_unstable();
        assert_eq!(got, vec!["r", "run"]);
        // `help ` (empty tail) offers every command key.
        let s = c.complete("help ", 5);
        let all = values(&s);
        assert!(all.contains(&"run") && all.contains(&"help"));
    }

    // ---- session-aware completion (the Arc<Mutex<Session>> bridge) ---------

    #[test]
    fn command_reads_live_session_state() {
        // An interactive session → the probe command sees `interactive == true`
        // through the shared lock and completes accordingly.
        let mut registry = Registry::new();
        registry.register(Arc::new(SessionProbeCmd));
        let session = Session::new(Config::default(), true);
        let mut c = MtuiCompleter::new(Arc::new(registry), Arc::new(Mutex::new(session)));
        assert_eq!(values(&c.complete("shell ", 6)), vec!["interactive"]);
    }

    #[test]
    fn completer_reflects_headless_session() {
        // The mirror case: a headless session flips the probe's answer, proving
        // the completer reads the *live* session, not a baked-in snapshot.
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
        // Two leading spaces; "run r" starts at byte 2, "r" partial at byte 6.
        let line = "  run r";
        let s = c.complete(line, line.len());
        // FixedCmd.complete("r", …) → "reboot".
        assert_eq!(values(&s), vec!["reboot"]);
        // Span must index the ORIGINAL buffer: the "r" partial is bytes 6..7.
        assert_eq!(s[0].span, Span::new(6, 7));
        assert_eq!(&line[s[0].span.start..s[0].span.end], "r");
    }
}
