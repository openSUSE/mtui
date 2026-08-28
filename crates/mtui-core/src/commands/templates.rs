//! The `list_templates` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;
use crate::template_registry::TemplateRow;

/// Lists all loaded templates, marking the active one.
///
/// Shows each template's RRID, connected host count and workflow mode. In the
/// REPL the active one — what plain action commands act on — is marked with a
/// leading `*`; under MCP there is no client-addressable active pointer
/// (`switch` being REPL-only), so the marker is omitted. A template held by
/// another dispatch is listed `busy` rather than omitted — a missing row reads
/// as "not loaded" (#524). Reads the whole registry rather than one template,
/// so it runs once ([`Scope::Single`]).
pub struct ListTemplates;

#[async_trait]
impl Command for ListTemplates {
    fn name(&self) -> &'static str {
        "list_templates"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists all loaded templates, marking the active one.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Walks the whole registry, never the report it was handed; an entry it
        // cannot lock is listed `busy`, not read off the sentinel or dropped.
        false
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrids = session.templates.rrids();
        if rrids.is_empty() {
            session.display.println("no templates loaded");
            return Ok(());
        }

        // Under MCP the active pointer is hidden state the client cannot
        // address, so it gets no marker.
        let active = session.templates.active_rrid().map(str::to_owned);
        let is_repl = session.is_repl;

        // Snapshotted so the report borrow does not overlap the display's
        // mutable borrow.
        let rows: Vec<(String, TemplateRow)> = rrids
            .iter()
            .filter_map(|rrid| session.template_row(rrid).map(|row| (rrid.clone(), row)))
            .collect();

        for (rrid, row) in rows {
            let marker = if is_repl && active.as_deref() == Some(rrid.as_str()) {
                "*"
            } else {
                " "
            };
            let detail = match row {
                TemplateRow::Read(hosts, mode) => format!("hosts: {hosts}  mode: {mode}"),
                // Never drop the row: absent from the listing reads as "not
                // loaded", which is the #524 collapse in another costume.
                TemplateRow::Busy => "busy (in use by another command)".to_owned(),
            };
            session
                .display
                .println(&format!("{marker} {rrid}  {detail}"));
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
        assert!(!out.contains('*'), "{out}");
    }

    /// A held entry used to collapse into `None` and vanish from the listing,
    /// telling the operator the template is not loaded (#524's shape).
    #[tokio::test]
    async fn held_template_is_listed_busy_not_dropped() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2", "h3"], "ok"));
        // Someone else's dispatch holds 2:2; the session holds no guard.
        session.release_active_guard();
        let entry = session
            .templates
            .handle("SUSE:Maintenance:2:2")
            .expect("just added");
        let _held = entry.try_lock_owned().expect("uncontended");

        let args = matches(&ListTemplates, &[]);
        ListTemplates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("SUSE:Maintenance:2:2  busy"), "{out}");
        // Anti-vacuity: the unheld one still reports its real host count.
        assert!(out.contains("SUSE:Maintenance:1:1  hosts: 1"), "{out}");
    }

    #[tokio::test]
    async fn interactive_marks_active_template() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.is_repl = true;
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert!(
            session.activate("SUSE:Maintenance:1:1").is_active(),
            "seeded template must activate"
        );
        let args = matches(&ListTemplates, &[]);
        ListTemplates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("* SUSE:Maintenance:1:1"), "{out}");
        assert!(out.contains("  SUSE:Maintenance:2:2"), "{out}");
    }
}
