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
    /// than a genuine usage error (argparse exit 2). The headless entrypoint
    /// ([`run_once`](crate::entrypoint::run_once)) reads it to pick the right
    /// process exit code; the REPL ignores it and just renders the message.
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
    // `help` is intercepted here (before command lookup) because listing
    // commands and rendering a target's `--help` both need the `Registry`, which
    // the `Command` trait does not hand to `call()`. This is the engine-layer
    // analogue of the REPL intercepting `shell`; the registered `Help` command
    // still exists for listing/completion/deny-list purposes.
    if name == "help" {
        return render_help(registry, session, argv);
    }

    let command = registry
        .get(name)
        .ok_or_else(|| EngineError::UnknownCommand(name.to_owned()))?;
    let matches = parse_command(command.as_ref(), argv)?;
    command.run(session, &matches).await.map_err(Into::into)
}

/// Column layout for the no-arg `help` listing (mirrors upstream
/// `help.py:_COLUMN_WIDTH`/`_COLUMNS_PER_ROW`).
const HELP_COLUMN_WIDTH: usize = 22;
const HELP_COLUMNS_PER_ROW: usize = 4;

/// Implements the `help` command against the live [`Registry`].
///
/// With no argument, prints the command listing split into documented
/// ([`Command::about`] is `Some`) and undocumented buckets in fixed-width
/// columns. With a command name, prints that command's `--help` (the same text
/// `<cmd> --help` produces), or an error if the name is unknown.
fn render_help(
    registry: &Registry,
    session: &mut Session,
    argv: &[String],
) -> Result<(), EngineError> {
    // `help` takes at most one positional (the topic); reject extra args the way
    // a clap parser would, but tolerate a lone `-h/--help` on `help` itself.
    let topic = argv.iter().find(|a| !a.starts_with('-'));

    let Some(topic) = topic else {
        render_help_listing(registry, session);
        return Ok(());
    };

    let command = registry.get(topic).ok_or_else(|| EngineError::Parse {
        message: format!("No help available: '{topic}' is not a known command"),
        help_or_version: false,
    })?;
    let mut parser = command.configure(base_subcommand(command.name()));
    session
        .display
        .println(parser.render_long_help().to_string().trim_end());
    Ok(())
}

/// Prints the documented/undocumented command listing.
fn render_help_listing(registry: &Registry, session: &mut Session) {
    let mut documented: Vec<&str> = Vec::new();
    let mut undocumented: Vec<&str> = Vec::new();
    let mut names: Vec<&str> = registry.names().collect();
    names.sort_unstable();
    for name in names {
        match registry.get(name).and_then(|c| c.about()) {
            Some(_) => documented.push(name),
            None => undocumented.push(name),
        }
    }

    session
        .display
        .println("Documented commands (type help <topic>):");
    session.display.println(&"=".repeat(40));
    print_help_columns(session, &documented);

    if !undocumented.is_empty() {
        session.display.println("");
        session.display.println("Undocumented commands:");
        session.display.println(&"=".repeat(40));
        print_help_columns(session, &undocumented);
    }
}

/// Prints `names` in a fixed-width column grid, trailing spaces stripped
/// (mirrors upstream `help.py:_print_columns`).
fn print_help_columns(session: &mut Session, names: &[&str]) {
    for row in names.chunks(HELP_COLUMNS_PER_ROW) {
        let line: String = row
            .iter()
            .map(|n| format!("{n:<HELP_COLUMN_WIDTH$}"))
            .collect();
        session.display.println(line.trim_end());
    }
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

    /// A documented stub command (returns `Some` from `about`).
    struct DocCmd;

    #[async_trait]
    impl Command for DocCmd {
        fn name(&self) -> &'static str {
            "doc"
        }
        fn about(&self) -> Option<&'static str> {
            Some("a documented command")
        }
        async fn call(&self, _s: &mut Session, _a: &ArgMatches) -> CommandResult {
            Ok(())
        }
    }

    /// A registry with the `Help` command plus one documented and one
    /// undocumented stub, and a session whose display is captured.
    fn help_registry_and_session() -> (Registry, Session, crate::commands::testkit::Buffer) {
        let mut r = Registry::new();
        r.register(Arc::new(crate::commands::Help));
        r.register(Arc::new(DocCmd));
        r.register(Arc::new(EchoCmd::default()));
        let (s, buf) = crate::commands::testkit::empty_session();
        (r, s, buf)
    }

    #[tokio::test]
    async fn help_no_arg_lists_documented_and_undocumented() {
        let (r, mut s, buf) = help_registry_and_session();
        dispatch_line(&r, &mut s, "help").await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Documented commands (type help <topic>):"));
        assert!(out.contains("Undocumented commands:"));
        // `doc` returns Some(about) → documented; `echo` returns None →
        // undocumented; both must appear.
        assert!(out.contains("doc"));
        assert!(out.contains("echo"));
        // Documented section precedes the undocumented one.
        let doc_hdr = out.find("Documented commands").unwrap();
        let undoc_hdr = out.find("Undocumented commands").unwrap();
        assert!(doc_hdr < undoc_hdr);
    }

    #[tokio::test]
    async fn help_no_arg_omits_undocumented_header_when_all_documented() {
        let mut r = Registry::new();
        r.register(Arc::new(crate::commands::Help)); // documented
        r.register(Arc::new(DocCmd)); // documented
        let (mut s, buf) = crate::commands::testkit::empty_session();
        dispatch_line(&r, &mut s, "help").await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Documented commands"));
        assert!(!out.contains("Undocumented commands"));
    }

    #[tokio::test]
    async fn help_topic_renders_that_commands_help() {
        let (r, mut s, buf) = help_registry_and_session();
        dispatch_line(&r, &mut s, "help doc").await.unwrap();
        let out = buf.contents();
        // Same surface as `doc --help`: usage line + the shared template flags
        // (`render_long_help` reflects the clap parser, i.e. what `<cmd> --help`
        // prints — not `Command::about`, which drives the listing split only).
        assert!(out.contains("Usage:"));
        assert!(out.contains("doc"));
        assert!(out.contains("--all-templates"));
    }

    #[tokio::test]
    async fn help_unknown_topic_is_a_parse_error_not_a_listing() {
        let (r, mut s, buf) = help_registry_and_session();
        let err = dispatch_line(&r, &mut s, "help nosuch").await.unwrap_err();
        assert!(matches!(
            err,
            EngineError::Parse { help_or_version: false, ref message }
                if message.contains("No help available") && message.contains("nosuch")
        ));
        // Nothing was rendered as a listing.
        assert!(buf.contents().is_empty());
    }
}
