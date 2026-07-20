//! The `list_bugs` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::complete_with_templates;
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

    fn about(&self) -> Option<&'static str> {
        Some("Lists related bugs and their Bugzilla/Jira URLs.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[&["-p", "--pool"]], Vec::new(), line, text)
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
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListBugs.name(), "list_bugs");
        assert_eq!(ListBugs.scope(), Scope::Fanout);
    }

    #[test]
    fn complete_offers_pool_flag_and_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = ListBugs.complete(&session, "", "list_bugs ");
        assert!(
            out.contains(&"-p".to_owned()) && out.contains(&"--pool".to_owned()),
            "{out:?}"
        );
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
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
