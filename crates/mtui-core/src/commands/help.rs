//! The `help` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Lists available commands, or shows detailed help for one command.
///
/// Ports upstream `mtui.commands.help.Help`. With no argument it lists every
/// registered command (documented vs undocumented buckets, fixed-width
/// columns); with a command name it prints that command's `--help`. Listing and
/// per-command help both need the command [`Registry`](crate::Registry), which
/// the [`Command`] trait does not hand to [`call`](Command::call); so `help` is
/// intercepted in the engine (`dispatch_argv`), where the registry is in hand —
/// mirroring how the REPL intercepts `shell`. This registered command exists so
/// `help` appears in listings, completion, and the MCP deny-list check; its
/// [`call`](Command::call) body is only reached if the intercept is bypassed and
/// then defers, exactly like `shell`'s headless fallback. REPL-only — on the MCP
/// deny-list.
pub struct Help;

#[async_trait]
impl Command for Help {
    fn name(&self) -> &'static str {
        "help"
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn about(&self) -> Option<&'static str> {
        Some("List commands, or show help for one command.")
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("command")
                .num_args(0..=1)
                .value_name("COMMAND")
                .help("command to show help for; omit to list all commands"),
        )
    }

    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        // Reached only if the engine intercept is bypassed: `help` needs the
        // command registry, which this body has no handle to. Defer with a clear
        // message rather than fabricate a registry (same shape as `shell`).
        Err(CommandError::Other(
            "help is available in the interactive REPL".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_scope_and_about() {
        assert_eq!(Help.name(), "help");
        assert_eq!(Help.scope(), Scope::Single);
        assert!(Help.about().is_some());
    }

    #[tokio::test]
    async fn call_defers_to_the_repl_intercept() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Help, &[]);
        let err = Help.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(msg) if msg.contains("interactive REPL")));
    }

    #[tokio::test]
    async fn call_accepts_a_command_argument() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Help, &["run"]);
        // The body defers regardless; this asserts arg parsing accepts the
        // positional so the intercept path receives it.
        assert_eq!(
            args.get_one::<String>("command").map(String::as_str),
            Some("run")
        );
        let err = Help.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
