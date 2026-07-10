//! The `show_update_repos` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Shows the update repositories that are valid for the current update.
///
/// Ports upstream `mtui.commands.showrepos.Showrepos`. Reads the active report's
/// [`update_repos`](mtui_testreport::TestReportBase::update_repos) and lists them
/// through the display.
pub struct ShowUpdateRepos;

#[async_trait]
impl Command for ShowUpdateRepos {
    fn name(&self) -> &'static str {
        "show_update_repos"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Shows the update repositories that are valid for the current update.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        // Snapshot the (product, repo) pairs first so the display's mutable
        // borrow does not overlap the report's immutable borrow.
        let mut repos: Vec<(mtui_types::SystemProduct, String)> = session
            .metadata()
            .base()
            .update_repos
            .iter()
            .map(|(p, r)| (p.clone(), r.clone()))
            .collect();
        repos.sort_by(|a, b| a.0.name.cmp(&b.0.name).then(a.0.version.cmp(&b.0.version)));
        session.display.list_update_repos(&repos);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ShowUpdateRepos.name(), "show_update_repos");
        assert_eq!(ShowUpdateRepos.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn empty_report_lists_nothing() {
        let (mut session, buf) = empty_session();
        let args = matches(&ShowUpdateRepos, &[]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().is_empty());
    }

    #[tokio::test]
    async fn lists_update_repos_sorted() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // Seed the active report's update_repos.
        let base = session.templates.active_mut().base_mut();
        base.update_repos.insert(
            mtui_types::SystemProduct::new("SLES", "15.5", "x86_64"),
            "http://repo/b".to_owned(),
        );
        base.update_repos.insert(
            mtui_types::SystemProduct::new("Basesystem", "15.5", "x86_64"),
            "http://repo/a".to_owned(),
        );
        let args = matches(&ShowUpdateRepos, &[]);
        ShowUpdateRepos.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        // Sorted by product name: Basesystem before SLES.
        let a = out.find("Basesystem").expect("Basesystem listed");
        let b = out.find("SLES").expect("SLES listed");
        assert!(a < b, "expected sorted order:\n{out}");
        assert!(out.contains("http://repo/a") && out.contains("http://repo/b"));
    }
}
