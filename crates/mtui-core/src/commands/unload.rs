//! The `unload` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Unloads one loaded template, closing only its host connections.
///
/// Other templates are untouched; unloading the active one promotes the next
/// remaining. It names its own target RRID, so it runs once ([`Scope::Single`])
/// — otherwise it would fan out under MCP and fail the second pass with a
/// not-loaded error.
pub struct Unload;

#[async_trait]
impl Command for Unload {
    fn name(&self) -> &'static str {
        "unload"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Unloads one loaded template, closing only its host connections.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn mutates_registry(&self) -> bool {
        true
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("rrid")
                .required(true)
                .value_name("RRID")
                .help("RRID of the loaded template to unload"),
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
        if !session.templates.contains(&rrid) {
            return Err(CommandError::TemplateNotLoaded(rrid));
        }
        // `remove` locks the target entry to tear it down, self-deadlocking when
        // that is the entry this session's guard holds. The registry pointer is
        // left alone, so `remove` still promotes the survivor.
        session.release_active_guard();
        // Removal releases the arbiter claim + remote pool/operation locks and
        // closes the hosts (bounded) before the entry drops. As in `quit`,
        // teardown failures are logged and unload still succeeds.
        let removed = session.templates.remove(&rrid).await;
        for (host, err) in &removed.failed {
            tracing::warn!("failed to disconnect from {host}: {err}");
        }
        for host in &removed.stragglers {
            tracing::warn!("still disconnecting from {host}");
        }
        let mut msg = format!("unloaded {rrid}");
        if !removed.failed.is_empty() || !removed.stragglers.is_empty() {
            msg.push_str(&format!(
                " ({} failed, {} still disconnecting)",
                removed.failed.len(),
                removed.stragglers.len()
            ));
        }
        session.display.println(&msg);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Unload.name(), "unload");
        assert_eq!(Unload.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn unload_loaded_template_removes_it() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        let args = matches(&Unload, &["SUSE:Maintenance:2:2"]);
        Unload.call(&mut session, &args).await.unwrap();
        assert!(!session.templates.contains("SUSE:Maintenance:2:2"));
        assert!(session.templates.contains("SUSE:Maintenance:1:1"));
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("unloaded SUSE:Maintenance:2:2"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn unload_releases_pool_claims_and_closes_hosts() {
        // Unload must release the arbiter ownership, not just drop the entry.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));

        let owner = (
            session.templates.id().to_owned(),
            "SUSE:Maintenance:1:1".to_owned(),
        );
        let arbiter = mtui_hosts::get_arbiter();
        assert!(arbiter.try_acquire("unload-claim-h1", &owner));
        // The active template, so mutate it through the session's handle rather
        // than a locked registry entry.
        session
            .metadata_mut()
            .base_mut()
            .pool_claims
            .insert("unload-claim-h1".to_owned());

        let args = matches(&Unload, &["SUSE:Maintenance:1:1"]);
        Unload.call(&mut session, &args).await.unwrap();

        assert!(!session.templates.contains("SUSE:Maintenance:1:1"));
        assert_eq!(
            session.templates.active_rrid(),
            Some("SUSE:Maintenance:2:2"),
            "the survivor is promoted"
        );
        assert!(
            arbiter.owner_of("unload-claim-h1").is_none(),
            "the unloaded report's arbiter claim is released"
        );
    }

    #[tokio::test]
    async fn unload_missing_template_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Unload, &["SUSE:Maintenance:9:9"]);
        let err = Unload.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(
            err,
            CommandError::TemplateNotLoaded(r) if r == "SUSE:Maintenance:9:9"
        ));
    }

    #[test]
    fn complete_offers_loaded_rrids() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let candidates = Unload.complete(&session, "SUSE", "unload SUSE");
        assert_eq!(candidates, vec!["SUSE:Maintenance:1:1".to_owned()]);
    }
}
