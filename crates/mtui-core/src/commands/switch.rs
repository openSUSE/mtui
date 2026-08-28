//! The `switch` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Switches the active template — the one plain action commands act on — to
/// another loaded one.
///
/// It names its own target RRID, so it runs exactly once ([`Scope::Single`]),
/// never auto-fanned-out. REPL-only, the active pointer being meaningful only in
/// the interactive shell, so it is on the MCP deny-list.
pub struct Switch;

#[async_trait]
impl Command for Switch {
    fn name(&self) -> &'static str {
        "switch"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Switches the active template to another loaded one.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Re-points the active pointer; reads no report.
        false
    }

    fn requires_canonical_session(&self, _argv: &[String]) -> bool {
        true
    }

    /// The whole point of the command is to leave the pointer moved.
    fn repoints_active(&self) -> bool {
        true
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("rrid")
                .required(true)
                .value_name("RRID")
                .help("RRID of the loaded template to make active"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .templates
            .rrids()
            .into_iter()
            .filter(|r| r.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = args
            .get_one::<String>("rrid")
            .expect("rrid is required")
            .clone();
        if session.templates.set_active(&rrid) {
            Ok(())
        } else {
            Err(CommandError::TemplateNotLoaded(rrid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Switch.name(), "switch");
        assert_eq!(Switch.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn switch_to_loaded_template_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // The first stays active until the switch.
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:1:1")
        );
        let args = matches(&Switch, &["SUSE:Maintenance:2:2"]);
        Switch.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:2:2")
        );
    }

    #[tokio::test]
    async fn switch_to_unloaded_template_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Switch, &["SUSE:Maintenance:9:9"]);
        let err = Switch.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(
            err,
            CommandError::TemplateNotLoaded(r) if r == "SUSE:Maintenance:9:9"
        ));
    }

    /// The issue's reproducer: `switch` through `Command::run`, not `call`
    /// directly — `run` is what restores the pre-dispatch pointer, so only
    /// driving it this way can observe the revert `call`-only tests can't see.
    #[tokio::test]
    async fn switch_through_run_moves_active_pointer() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:1:1")
        );
        let args = matches(&Switch, &["SUSE:Maintenance:2:2"]);
        Switch.run(&mut session, &args).await.unwrap();
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:2:2")
        );
        assert!(
            session.active_report_is_guarded("SUSE:Maintenance:2:2"),
            "the guard must follow the pointer, or metadata() keeps answering \
             about the old report"
        );
    }

    #[tokio::test]
    async fn switch_through_run_to_unloaded_leaves_active_intact() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Switch, &["SUSE:Maintenance:9:9"]);
        let err = Switch.run(&mut session, &args).await.unwrap_err();
        assert!(matches!(
            err,
            CommandError::TemplateNotLoaded(r) if r == "SUSE:Maintenance:9:9"
        ));
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:1:1")
        );
        assert!(session.active_report_is_guarded("SUSE:Maintenance:1:1"));
    }

    #[test]
    fn complete_offers_loaded_rrids() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let candidates = Switch.complete(&session, "SUSE", "switch SUSE");
        assert_eq!(candidates, vec!["SUSE:Maintenance:1:1".to_owned()]);
    }
}
