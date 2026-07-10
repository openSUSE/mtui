//! The `unload` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Unloads one loaded template, closing only its host connections.
///
/// Ports upstream `mtui.commands.unload.Unload`. Other loaded templates are left
/// untouched; if the unloaded template was active, the registry promotes the
/// next remaining one. It names its own target RRID and removes exactly that
/// template, so it runs once ([`Scope::Single`]) regardless of how many
/// templates are loaded — without this it would fan out under MCP and fail on
/// the second pass with a not-loaded error.
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
        session.templates.remove(&rrid);
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
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        let args = matches(&Unload, &["SUSE:Maintenance:2:2"]);
        Unload.call(&mut session, &args).await.unwrap();
        assert!(!session.templates.contains("SUSE:Maintenance:2:2"));
        assert!(session.templates.contains("SUSE:Maintenance:1:1"));
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
