//! The `list_bugs` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists related bugs and their Bugzilla/Jira URLs.
///
/// Ports upstream `mtui.commands.simplelists.ListBugs`. Reads the loaded
/// report's Bugzilla and Jira id→title maps (upstream `metadata.list_bugs`,
/// which forwards `self.bugs`/`self.jira`) and renders them through the
/// display's `list_bugs` sink together with `config.bugzilla_url`. With nothing
/// loaded the maps are empty, so the sink prints the "No bugs…"/"No Jira…"
/// sentinels.
pub struct ListBugs;

#[async_trait]
impl Command for ListBugs {
    fn name(&self) -> &'static str {
        "list_bugs"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let (bugs, jira) = session.metadata().bug_maps();
        let url = session.config.bugzilla_url.clone();
        session.display.list_bugs(&bugs, &jira, &url);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListBugs.name(), "list_bugs");
        assert_eq!(ListBugs.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn empty_report_renders_bug_query_and_no_jira() {
        // The null report has genuinely empty bug/jira maps (not the upstream
        // `[""]` sentinel), so the display prints the (empty) Buglist query URL
        // and the "No Jira issues" sentinel. Exercises the command wiring.
        let (mut session, buf) = empty_session();
        let args = matches(&ListBugs, &[]);
        ListBugs.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Buglist:"), "{out}");
        assert!(
            out.contains("No Jira issues associated with Release Request."),
            "{out}"
        );
    }
}
