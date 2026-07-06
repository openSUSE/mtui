//! Line → dispatch engine.
//!
//! Ports the dispatch half of upstream `mtui.commands.Command.parse_args` +
//! `run`: split a raw input line into argv, resolve the command by name (or
//! alias) against the [`Registry`], parse its arguments, and await
//! [`Command::run`] (which drives the template fan-out landed in P5.1).
//!
//! Two entry points share one core so both driving surfaces reuse the same
//! engine (`AGENTS.md`: MCP dispatches through the *same engine* as the REPL):
//!
//! * [`dispatch_line`] — the REPL path: takes a raw line, `shlex`-splits it
//!   (upstream `shlex.split`), and treats the first token as the command name.
//! * [`dispatch_argv`] — the MCP path: takes an already-structured command name
//!   and argv, so a client that already has parsed kwargs need not serialise
//!   them back into a string just to re-split them.
//!
//! Errors never abort the process. Upstream `argparse` calls `sys.exit` on a
//! usage error or `--help`; clap defaults to the same. The engine instead uses
//! clap's non-exiting parse API and returns a typed [`EngineError`], which the
//! REPL renders and the MCP surface maps to a tool error.

use clap::ArgMatches;

use crate::command::Command;
use crate::error::CommandError;
use crate::registry::Registry;
use crate::session::Session;

/// A failure raised while dispatching an input line or argv.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The first token named no registered command or alias (upstream prints
    /// `*** Unknown syntax: <line>`; we name the command).
    #[error("Unknown command: {0}")]
    UnknownCommand(String),

    /// The line could not be split into argv — unbalanced quotes (upstream
    /// `p.error(f"invalid syntax: {e}")`).
    #[error("invalid syntax: {0}")]
    Syntax(String),

    /// The command's arguments failed to parse or `--help`/`--version` was
    /// requested. Carries clap's already-rendered message so the caller can
    /// present it verbatim, exactly as argparse's usage text was shown.
    ///
    /// `help_or_version` records whether the "error" was actually clap emitting
    /// `--help`/`--version` text (a success in argparse terms, exit 0) rather
    /// than a genuine usage error (argparse exit 2). The non-interactive
    /// entrypoint ([`run_once`](crate::entrypoint::run_once)) reads it to pick
    /// the right process exit code; the REPL ignores it and just renders the
    /// message.
    #[error("{message}")]
    Parse {
        /// clap's already-rendered help/usage text.
        message: String,
        /// `true` iff this is `--help`/`--version` output, not a usage error.
        help_or_version: bool,
    },

    /// The command ran but failed. Bridges the command-layer error hierarchy so
    /// a single `EngineError` covers the whole dispatch path.
    #[error(transparent)]
    Command(#[from] CommandError),
}

/// Dispatches a raw input line: `shlex`-split, then [`dispatch_argv`].
///
/// A blank or whitespace-only line is a no-op (`Ok(())`), matching a REPL that
/// simply re-prompts on an empty line.
///
/// # Errors
///
/// [`EngineError::Syntax`] if the line cannot be split (unbalanced quotes),
/// otherwise whatever [`dispatch_argv`] returns.
pub async fn dispatch_line(
    registry: &Registry,
    session: &mut Session,
    line: &str,
) -> Result<(), EngineError> {
    let tokens =
        shlex::split(line).ok_or_else(|| EngineError::Syntax("unbalanced quotes".to_owned()))?;
    let Some((name, argv)) = tokens.split_first() else {
        return Ok(());
    };
    dispatch_argv(registry, session, name, argv).await
}

/// Dispatches an already-tokenised command: resolve, parse, run.
///
/// # Errors
///
/// * [`EngineError::UnknownCommand`] if `name` matches no command or alias.
/// * [`EngineError::Parse`] if argument parsing fails or help/version is
///   requested (clap's message is carried through, the process is not exited).
/// * [`EngineError::Command`] if the command body fails.
pub async fn dispatch_argv(
    registry: &Registry,
    session: &mut Session,
    name: &str,
    argv: &[String],
) -> Result<(), EngineError> {
    let command = registry
        .get(name)
        .ok_or_else(|| EngineError::UnknownCommand(name.to_owned()))?;
    let matches = parse_command(command.as_ref(), argv)?;
    command.run(session, &matches).await.map_err(Into::into)
}

/// Builds `command`'s clap parser and parses `argv` into [`ArgMatches`] without
/// ever exiting the process.
fn parse_command(command: &dyn Command, argv: &[String]) -> Result<ArgMatches, EngineError> {
    use clap::error::ErrorKind;

    let parser = command.configure(base_subcommand(command.name()));
    parser.try_get_matches_from(argv).map_err(|e| {
        let help_or_version =
            matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion);
        EngineError::Parse {
            message: e.to_string(),
            help_or_version,
        }
    })
}

/// The base clap parser shared by every command.
///
/// * `no_binary_name(true)` — argv is the command's own arguments; the command
///   name is not a leading binary to strip.
/// * The template-selection flags (`-T/--template`, `--all-templates`) every
///   command honours through [`Command::run`]'s fan-out resolver are declared
///   here so a command's own [`configure`](Command::configure) need only add its
///   specific arguments.
fn base_subcommand(name: &'static str) -> clap::Command {
    clap::Command::new(name)
        .no_binary_name(true)
        .arg(
            clap::Arg::new("template")
                .short('T')
                .long("template")
                .value_name("RRID")
                .help("RRID of a single loaded template to act on"),
        )
        .arg(
            clap::Arg::new("all_templates")
                .long("all-templates")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("template")
                .help("Act on every loaded template"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Scope};
    use crate::error::CommandResult;
    use async_trait::async_trait;
    use mtui_config::Config;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records how many times it ran and the last positional arg it saw.
    #[derive(Default)]
    struct EchoCmd {
        runs: Arc<AtomicUsize>,
        last: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Command for EchoCmd {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["e"]
        }
        fn scope(&self) -> Scope {
            Scope::Single
        }
        fn configure(&self, cmd: clap::Command) -> clap::Command {
            cmd.arg(clap::Arg::new("word").num_args(0..=1))
        }
        async fn call(&self, _session: &mut Session, args: &ArgMatches) -> CommandResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = args.get_one::<String>("word").cloned();
            Ok(())
        }
    }

    fn session() -> Session {
        Session::new(Config::default(), true)
    }

    fn registry_with_echo() -> (
        Registry,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Option<String>>>,
    ) {
        let runs = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(std::sync::Mutex::new(None));
        let cmd = EchoCmd {
            runs: Arc::clone(&runs),
            last: Arc::clone(&last),
        };
        let mut r = Registry::new();
        r.register(Arc::new(cmd));
        (r, runs, last)
    }

    #[tokio::test]
    async fn dispatch_line_runs_command_by_name() {
        let (r, runs, last) = registry_with_echo();
        let mut s = session();
        dispatch_line(&r, &mut s, "echo hello").await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(last.lock().unwrap().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn dispatch_line_resolves_alias() {
        let (r, runs, _) = registry_with_echo();
        let mut s = session();
        dispatch_line(&r, &mut s, "e hi").await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_argv_reaches_same_body_as_line() {
        let (r, _, last) = registry_with_echo();
        let mut s = session();
        dispatch_argv(&r, &mut s, "echo", &["structured".to_owned()])
            .await
            .unwrap();
        assert_eq!(last.lock().unwrap().as_deref(), Some("structured"));
    }

    #[tokio::test]
    async fn empty_line_is_a_noop() {
        let (r, runs, _) = registry_with_echo();
        let mut s = session();
        dispatch_line(&r, &mut s, "   ").await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_command_is_reported() {
        let (r, _, _) = registry_with_echo();
        let mut s = session();
        let err = dispatch_line(&r, &mut s, "nope").await.unwrap_err();
        assert!(matches!(err, EngineError::UnknownCommand(c) if c == "nope"));
    }

    #[tokio::test]
    async fn unbalanced_quotes_is_a_syntax_error() {
        let (r, _, _) = registry_with_echo();
        let mut s = session();
        let err = dispatch_line(&r, &mut s, "echo \"unterminated")
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Syntax(_)));
    }

    #[tokio::test]
    async fn bad_flag_is_a_parse_error_not_a_panic() {
        let (r, runs, _) = registry_with_echo();
        let mut s = session();
        let err = dispatch_line(&r, &mut s, "echo --no-such-flag")
            .await
            .unwrap_err();
        // A genuine usage error, not `--help`/`--version` output.
        assert!(matches!(
            err,
            EngineError::Parse {
                help_or_version: false,
                ..
            }
        ));
        // The body never ran.
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn help_flag_surfaces_as_parse_without_exiting() {
        let (r, runs, _) = registry_with_echo();
        let mut s = session();
        // clap emits `--help` text as an Error; the engine must carry it, not
        // exit the process, and flag it as help/version output (exit 0).
        let err = dispatch_line(&r, &mut s, "echo --help").await.unwrap_err();
        assert!(matches!(
            err,
            EngineError::Parse {
                help_or_version: true,
                ..
            }
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn template_flag_for_unloaded_rrid_surfaces_command_error() {
        let (r, _, _) = registry_with_echo();
        let mut s = session();
        let err = dispatch_line(&r, &mut s, "echo -T SUSE:Maintenance:1:1")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::Command(CommandError::TemplateNotLoaded(rrid)) if rrid == "SUSE:Maintenance:1:1"
        ));
    }
}
