//! The `downgrade` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::perform::{PerformOp, drive};
use super::support::{add_hosts_arg, complete_fanout};
use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Downgrades all related packages to the last released version.
///
/// Ports upstream `mtui.commands.downgrade.Downgrade` (including PR #336). Drives
/// [`TestReport::perform_downgrade`](mtui_testreport::TestReport::perform_downgrade),
/// which removes the issue repos, probes each package's available downgrade
/// version in a single `zypper se -s` invocation, downgrades (per-package for
/// non-transactional hosts, combined for transactional), runs the check, and
/// reboots transactional hosts.
///
/// The post-downgrade verdict upstream performs (upstream `commands/downgrade.py`)
/// is done by the workflow itself: `perform_downgrade` re-queries versions,
/// rotates `before = after; after = current` per package, and — crucially —
/// **aborts loudly** rather than half-rolling back silently. A dead version
/// probe (an SSH no-output timeout records exit `-1`) or a dead downgrade command
/// fails the host; any package still at or above the update's `required` version
/// after the run is named per host at ERROR. Because `drive` maps that
/// [`UpdateError`](mtui_testreport::UpdateError) onto a `CommandError`, a
/// half-rollback fails the command for both the REPL and headless (MCP) callers
/// instead of returning a success-looking log.
///
/// Warning: this command cannot work for new packages (they have no released
/// version to go back to, so they always appear in the not-downgraded list).
pub struct Downgrade;

#[async_trait]
impl Command for Downgrade {
    fn name(&self) -> &'static str {
        "downgrade"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Downgrades all related packages to the last released version.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let packages = session.metadata().get_package_list();
        drive(session, args, PerformOp::Downgrade(packages)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{
        empty_session, matches, session_with_failing_perform, session_with_hosts,
    };
    use crate::error::CommandError;

    #[test]
    fn complete_offers_target_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Downgrade.complete(&session, "", "downgrade ");
        assert!(
            out.contains(&"-t".to_owned()) && out.contains(&"h1".to_owned()),
            "{out:?}"
        );
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Downgrade.name(), "downgrade");
        assert_eq!(Downgrade.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn over_loaded_report_succeeds() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Downgrade, &[]);
        Downgrade.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().names(), vec!["h1"]);
        assert!(
            buf.contents().contains("downgrade completed on h1"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn failure_errors_and_names_host() {
        let (mut session, buf) = session_with_failing_perform("SUSE:Maintenance:1:1", &["h1"]);
        let args = matches(&Downgrade, &[]);
        let err = Downgrade.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("h1")));
        assert!(!buf.contents().contains("completed"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        // Loaded report but no hosts: passes the requires_update guard, then the
        // empty selection yields NoRefhostsDefined.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "ok");
        let args = matches(&Downgrade, &[]);
        let err = Downgrade.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn no_template_loaded_errors() {
        // No report loaded → requires_update guard fires first, mirroring
        // upstream @requires_update.
        let (mut session, _buf) = empty_session();
        let args = matches(&Downgrade, &[]);
        let err = Downgrade.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
