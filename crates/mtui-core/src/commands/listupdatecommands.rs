//! The `list_update_commands` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists the commands mtui would invoke to apply the update on the hosts.
///
/// Not implemented: `update` runs the per-host `updater` commands itself, and
/// this command only prints a placeholder.
pub struct ListUpdateCommands;

#[async_trait]
impl Command for ListUpdateCommands {
    fn name(&self) -> &'static str {
        "list_update_commands"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the commands mtui would invoke to apply the update on the hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        session
            .display
            .println("list_update_commands: not yet implemented");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListUpdateCommands.name(), "list_update_commands");
        assert_eq!(ListUpdateCommands.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn null_report_prints_not_yet_implemented() {
        let (mut session, buf) = empty_session();
        let args = matches(&ListUpdateCommands, &[]);
        ListUpdateCommands.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents()
                .contains("list_update_commands: not yet implemented"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn loaded_report_prints_not_yet_implemented() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListUpdateCommands, &[]);
        ListUpdateCommands.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents()
                .contains("list_update_commands: not yet implemented"),
            "{}",
            buf.contents()
        );
    }
}
