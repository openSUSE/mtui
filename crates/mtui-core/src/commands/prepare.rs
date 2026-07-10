//! The `prepare` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::perform::{PerformOp, drive};
use super::support::{add_hosts_arg, complete_fanout};
use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Installs missing packages and updates existing packages.
///
/// Ports upstream `mtui.commands.prepare.Prepare`. Drives
/// [`TestReport::perform_prepare`](mtui_testreport::TestReport::perform_prepare).
/// It is also run by the update procedure before applying the updates.
///
/// * `-f/--force` forces package installation (`--force-resolution`).
/// * `-i/--installed` prepares only already-installed packages.
/// * `-u/--update` enables the test update repositories.
pub struct Prepare;

#[async_trait]
impl Command for Prepare {
    fn name(&self) -> &'static str {
        "prepare"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Installs missing packages and updates existing packages.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("force")
                    .short('f')
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help("force package installation"),
            )
            .arg(
                Arg::new("installed")
                    .short('i')
                    .long("installed")
                    .action(ArgAction::SetTrue)
                    .help("prepare only installed packages"),
            )
            .arg(
                Arg::new("update")
                    .short('u')
                    .long("update")
                    .action(ArgAction::SetTrue)
                    .help("enable test update repositories"),
            )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(
            session,
            &[
                &["-i", "--installed"],
                &["-f", "--force"],
                &["-u", "--update"],
            ],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let packages = session.metadata().get_package_list();
        drive(
            session,
            args,
            PerformOp::Prepare {
                packages,
                force: args.get_flag("force"),
                testing: args.get_flag("update"),
                installed_only: args.get_flag("installed"),
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;

    #[test]
    fn complete_offers_own_flags_target_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = Prepare.complete(&session, "", "prepare ");
        for f in ["-t", "-i", "--installed", "-f", "--force", "-u", "--update"] {
            assert!(out.contains(&f.to_owned()), "missing {f}: {out:?}");
        }
        assert!(out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Prepare.name(), "prepare");
        assert_eq!(Prepare.scope(), Scope::Fanout);
    }

    #[test]
    fn flags_default_false_and_parse() {
        let d = matches(&Prepare, &[]);
        assert!(!d.get_flag("force") && !d.get_flag("installed") && !d.get_flag("update"));
        let a = matches(&Prepare, &["-f", "-i", "-u"]);
        assert!(a.get_flag("force") && a.get_flag("installed") && a.get_flag("update"));
    }

    #[tokio::test]
    async fn over_loaded_report_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Prepare, &["-u"]);
        Prepare.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        // Loaded report but no hosts: passes the requires_update guard, then the
        // empty selection yields NoRefhostsDefined.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "ok");
        let args = matches(&Prepare, &[]);
        let err = Prepare.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn no_template_loaded_errors() {
        // No report loaded (even with the empty session) → requires_update guard
        // fires first, mirroring upstream @requires_update.
        let (mut session, _buf) = empty_session();
        let args = matches(&Prepare, &[]);
        let err = Prepare.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
