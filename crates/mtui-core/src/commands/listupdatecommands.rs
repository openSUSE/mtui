//! The `list_update_commands` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists the commands mtui would invoke to apply the update on the hosts.
///
/// Ports upstream `mtui.commands.simplelists.ListUpdateCommands`, which calls
/// `metadata.list_update_commands(targets, println)`. The Rust
/// [`TestReport::list_update_commands`](mtui_testreport::TestReport) emits the
/// per-host update commands itself (a no-op for the null report); concrete
/// reports (SL/PI/OBS) render their updater command lines.
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
        // The report reads the group to render each host's update command line.
        // Snapshot-free: `list_update_commands` takes `&HostsGroup` and does not
        // touch the display, so no borrow conflict arises.
        let targets = session.targets();
        session.metadata().list_update_commands(targets);
        // `list_update_commands` is a no-op across every report type today
        // (unimplemented, bead mtui-rs-2d3.6): the delegate emits nothing, so
        // print an explicit placeholder instead of returning empty success.
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
