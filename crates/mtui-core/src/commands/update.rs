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

    fn about(&self) -> Option<&'static str> {
        Some("Applies the testing update to the target hosts.")
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
        // Upstream fires a desktop toast on both outcomes (`prompt.notify_user`);
        // the RRID is read before the drive so it survives an error path.
        let rrid = session.metadata().id();
        let result = drive(
            session,
            args,
            PerformOp::Update {
                noprepare,
                newpackage,
            },
        )
        .await;
        match &result {
            Ok(()) => {
                session.notify_user(&format!("updating {rrid} finished"), false);
            }
            Err(_) => {
                session.notify_user(&format!("updating {rrid} failed"), true);
            }
        }
        result
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
    async fn success_fires_finished_notification() {
        use std::sync::{Arc, Mutex};
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let seen = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let sink = Arc::clone(&seen);
        session.set_notify_sink(Box::new(move |msg: &str, err: bool| {
            sink.lock().unwrap().push((msg.to_owned(), err));
        }));
        let args = matches(&Update, &["--noprepare"]);
        Update.call(&mut session, &args).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one toast: {seen:?}");
        assert!(seen[0].0.contains("finished"), "got: {:?}", seen[0]);
        assert!(!seen[0].1, "success toast must not be error-class");
    }

    #[tokio::test]
    async fn no_notification_when_sink_unset() {
        // Headless (no sink): notify_user is a no-op, the command still succeeds.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Update, &["--noprepare"]);
        assert!(!session.notify_user("probe", false));
        Update.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Update, &[]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }
}
