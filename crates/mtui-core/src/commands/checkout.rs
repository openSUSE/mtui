//! The `checkout` command (SVN update of the template working copy).

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_testreport::{SvnRunner, TokioSvnRunner};

use super::support::complete_with_templates;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Updates the loaded template's files from SVN (`svn up`).
///
/// Ports upstream `mtui.commands.checkout.Checkout`, which runs `svn up` in the
/// report working directory. Requires a loaded report (upstream `@requires_update`);
/// with nothing loaded the report has no path and the command reports a clear
/// error rather than shelling out.
pub struct Checkout;

#[async_trait]
impl Command for Checkout {
    fn name(&self) -> &'static str {
        "checkout"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Updates the loaded template's files from SVN (`svn up`).")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let wd = session
            .metadata()
            .base()
            .report_wd()
            .map_err(|e| CommandError::Other(format!("no report loaded: {e}")))?;

        let runner = TokioSvnRunner;
        let outcome = runner
            .run(&["up".to_owned()], &wd)
            .await
            .map_err(|e| CommandError::Other(format!("svn up could not run: {e}")))?;
        if !outcome.success {
            return Err(CommandError::Other(format!(
                "svn up failed: {}",
                outcome.stderr.trim()
            )));
        }
        tracing::info!(wd = %wd.display(), "template updated from SVN");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Checkout.name(), "checkout");
        assert_eq!(Checkout.scope(), Scope::Fanout);
    }

    #[test]
    fn complete_offers_template_flags_and_rrids_but_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = Checkout.complete(&session, "", "checkout ");
        assert!(out.contains(&"-T".to_owned()), "{out:?}");
        assert!(out.contains(&"--all-templates".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        // No host names for a template-scoped command.
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[tokio::test]
    async fn no_report_errors_before_shelling_out() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Checkout, &[]);
        let err = Checkout.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
