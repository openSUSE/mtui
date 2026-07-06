//! The `quit` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Exits the interactive session.
///
/// Ports upstream `mtui.commands.quit.Quit`. Rather than routing process-exit
/// through the command-error channel, `quit` flips
/// [`Session::request_exit`](crate::Session::request_exit) and returns
/// `Ok(())`; the Phase-6 REPL checks
/// [`should_exit`](crate::Session::should_exit) after each line and breaks its
/// loop. It runs exactly once ([`Scope::Single`]) and is REPL-only — on the MCP
/// deny-list (a headless client has no session loop to quit).
pub struct Quit;

#[async_trait]
impl Command for Quit {
    fn name(&self) -> &'static str {
        "quit"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["exit", "EOF"]
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        session.request_exit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_aliases_and_single_scope() {
        assert_eq!(Quit.name(), "quit");
        assert_eq!(Quit.aliases(), &["exit", "EOF"]);
        assert_eq!(Quit.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn quit_requests_exit() {
        let (mut session, _buf) = empty_session();
        assert!(!session.should_exit());
        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());
    }
}
