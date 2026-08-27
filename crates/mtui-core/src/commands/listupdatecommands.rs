//! The `list_update_commands` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists the commands mtui would invoke to apply the update on the hosts.
///
/// [`TestReport::list_update_commands`](mtui_testreport::TestReport) emits them
/// itself: a no-op for the null report, the updater command lines for SL/PI/OBS.
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
        // Snapshot-free: it takes `&HostsGroup` and never touches the display,
        // so there is no borrow conflict.
        let targets = session.targets();
        session.metadata().list_update_commands(targets);
        // The delegate is a no-op for every report type today, so print a
        // placeholder rather than return an empty success.
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
