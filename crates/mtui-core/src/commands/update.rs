//! The `update` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::perform::{PerformOp, drive};
use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Applies the testing update to the target hosts.
///
/// Ports upstream `mtui.commands.update.Update`. Drives
/// [`TestReport::perform_update`](mtui_testreport::TestReport::perform_update),
/// which runs the full bespoke update flow (optional prepare, pre/post package
/// checks, repo add, per-host update + check, transactional reboot, two-phase
/// repo cleanup).
///
/// * `--noprepare` skips the initial prepare procedure.
/// * `--newpackage` installs new packages (a testing prepare) after the update.
pub struct Update;

#[async_trait]
impl Command for Update {
    fn name(&self) -> &'static str {
        "update"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("newpackage")
                    .long("newpackage")
                    .action(ArgAction::SetTrue)
                    .help("Install new packages after update"),
            )
            .arg(
                Arg::new("noprepare")
                    .long("noprepare")
                    .action(ArgAction::SetTrue)
                    .help("Skip prepare procedure"),
            )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let noprepare = args.get_flag("noprepare");
        let newpackage = args.get_flag("newpackage");
        drive(
            session,
            args,
            PerformOp::Update {
                noprepare,
                newpackage,
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
    fn name_and_fanout_scope() {
        assert_eq!(Update.name(), "update");
        assert_eq!(Update.scope(), Scope::Fanout);
    }

    #[test]
    fn flags_default_false_and_parse() {
        let d = matches(&Update, &[]);
        assert!(!d.get_flag("noprepare") && !d.get_flag("newpackage"));
        let a = matches(&Update, &["--noprepare", "--newpackage"]);
        assert!(a.get_flag("noprepare") && a.get_flag("newpackage"));
    }

    #[tokio::test]
    async fn over_loaded_report_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Update, &["--noprepare"]);
        Update.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Update, &[]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }
}
