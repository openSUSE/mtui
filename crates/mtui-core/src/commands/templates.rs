//! The `list_templates` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists all loaded templates, marking the active one.
///
/// Ports upstream `mtui.commands.templates.ListTemplates`. For each loaded
/// template the RRID, connected host count and workflow mode are shown. In the
/// REPL the active template (the one plain action commands act on) is marked
/// with a leading `*`; under MCP there is no client-addressable active pointer
/// (`switch` is REPL-only), so the marker is omitted.
///
/// Self-describing (reads the whole registry, not one template), so it runs once
/// ([`Scope::Single`]) rather than fanning out.
pub struct ListTemplates;

#[async_trait]
impl Command for ListTemplates {
    fn name(&self) -> &'static str {
        "list_templates"
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrids = session.templates.rrids();
        if rrids.is_empty() {
            session.display.println("no templates loaded");
            return Ok(());
        }

        // The active pointer is meaningful only in the interactive REPL; under
        // MCP it is hidden state the client cannot address, so no marker.
        let active = session.templates.active_rrid().map(str::to_owned);
        let interactive = session.interactive;

        // Snapshot the rows first so the report borrow does not overlap the
        // display's mutable borrow.
        let rows: Vec<(String, usize, &'static str)> = rrids
            .iter()
            .filter_map(|rrid| {
                session.templates.get(rrid).map(|r| {
                    (
                        rrid.clone(),
                        r.base().targets.len(),
                        r.base().workflow.as_str(),
                    )
                })
            })
            .collect();

        for (rrid, hosts, mode) in rows {
            let marker = if interactive && active.as_deref() == Some(rrid.as_str()) {
                "*"
            } else {
                " "
            };
            session
                .display
                .println(&format!("{marker} {rrid}  hosts: {hosts}  mode: {mode}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(ListTemplates.name(), "list_templates");
        assert_eq!(ListTemplates.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn empty_registry_says_none_loaded() {
        let (mut session, buf) = empty_session();
        let args = matches(&ListTemplates, &[]);
        ListTemplates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("no templates loaded"));
    }

    #[tokio::test]
    async fn headless_omits_active_marker() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2", "h3"], "ok"));
        let args = matches(&ListTemplates, &[]);
        ListTemplates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("  SUSE:Maintenance:1:1  hosts: 1"), "{out}");
        assert!(out.contains("  SUSE:Maintenance:2:2  hosts: 2"), "{out}");
        // interactive == false -> no `*` marker anywhere.
        assert!(!out.contains('*'), "{out}");
    }

    #[tokio::test]
    async fn interactive_marks_active_template() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.interactive = true;
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        // The most-recently added is active.
        session.templates.set_active("SUSE:Maintenance:1:1");
        let args = matches(&ListTemplates, &[]);
        ListTemplates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("* SUSE:Maintenance:1:1"), "{out}");
        assert!(out.contains("  SUSE:Maintenance:2:2"), "{out}");
    }
}
