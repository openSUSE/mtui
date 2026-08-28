//! The `help` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Lists available commands, or shows detailed help for one command.
///
/// With no argument it lists every registered command (documented vs
/// undocumented buckets, fixed-width columns); with a name it prints that
/// command's `--help`. Both need the command [`Registry`](crate::Registry),
/// which [`Command`] does not hand to [`call`](Command::call), so `help` is
/// intercepted in the engine (`dispatch_argv`) as the REPL intercepts `shell`.
/// It is registered anyway so it appears in listings, completion and the
/// deny-list check; [`call`](Command::call) only runs if the intercept is
/// bypassed, and then defers. REPL-only — on the MCP deny-list.
pub struct Help;

#[async_trait]
impl Command for Help {
    fn name(&self) -> &'static str {
        "help"
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Renders the command registry.
        false
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
        // Reached only if the intercept is bypassed: this body has no registry
        // handle, so defer rather than fabricate one.
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
        // The body defers regardless, so what matters is that parsing accepts
        // the positional the intercept path will receive.
        assert_eq!(
            args.get_one::<String>("command").map(String::as_str),
            Some("run")
        );
        let err = Help.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
