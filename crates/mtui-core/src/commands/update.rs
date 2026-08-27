//! The `update` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::perform::{PerformOp, drive};
use super::support::{add_hosts_arg, complete_fanout};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Applies the testing update to the target hosts.
///
/// Drives
/// [`TestReport::perform_update`](mtui_testreport::TestReport::perform_update):
/// optional prepare, pre/post package checks, repo add, per-host update + check,
/// transactional reboot, two-phase repo cleanup.
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

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(
            session,
            &[&["--noprepare"], &["--newpackage"]],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        tracing::info!("Updating");
        // Not-loaded first, so the refusal below can honestly say "the loaded
        // report".
        let _rrid = crate::commands::support::require_update(session)?;
        // #396: an empty package list would dispatch nothing and still print an
        // unqualified success.
        if session.metadata().get_package_list().is_empty() {
            return Err(CommandError::Other(
                "the loaded report names no package versions, so update has nothing to \
                 install; refusing to report success — check `list_packages -w` and the \
                 report metadata"
                    .to_owned(),
            ));
        }
        let noprepare = args.get_flag("noprepare");
        let newpackage = args.get_flag("newpackage");
        // Read before the drive so it survives an error path.
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
                tracing::info!("done");
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
    use crate::commands::testkit::{
        empty_session, matches, session_with_failing_update, session_with_hosts,
    };
    use crate::error::CommandError;

    /// Gets past the #396 pre-flight, which refuses a report naming no package
    /// versions.
    fn seed_package(session: &mut crate::session::Session) {
        session.metadata_mut().base_mut().packages.insert(
            "standard".to_owned(),
            std::collections::HashMap::from([("pkg-a".to_owned(), "1.0".to_owned())]),
        );
    }

    #[test]
    fn complete_offers_own_flags_target_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = Update.complete(&session, "", "update ");
        for f in ["-t", "--noprepare", "--newpackage"] {
            assert!(out.contains(&f.to_owned()), "missing {f}: {out:?}");
        }
        assert!(out.contains(&"h1".to_owned()), "{out:?}");
    }

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
        seed_package(&mut session);
        let args = matches(&Update, &["--noprepare"]);
        Update.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn success_fires_finished_notification() {
        use std::sync::{Arc, Mutex};
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        seed_package(&mut session);
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
        // Headless (no sink): notify_user is a no-op and the command succeeds.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        seed_package(&mut session);
        let args = matches(&Update, &["--noprepare"]);
        assert!(!session.notify_user("probe", false));
        Update.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn failure_fires_failed_notification_and_returns_err() {
        use std::sync::{Arc, Mutex};
        // A failing perform_update must surface as Err and fire exactly one
        // *error*-class toast.
        let (mut session, _buf) = session_with_failing_update("SUSE:Maintenance:1:1", &["h1"]);
        seed_package(&mut session);
        let seen = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let sink = Arc::clone(&seen);
        session.set_notify_sink(Box::new(move |msg: &str, err: bool| {
            sink.lock().unwrap().push((msg.to_owned(), err));
        }));
        let args = matches(&Update, &["--noprepare"]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)), "got: {err:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one toast: {seen:?}");
        assert!(seen[0].0.contains("failed"), "got: {:?}", seen[0]);
        assert!(seen[0].1, "failure toast must be error-class");
    }

    /// #396: a metadata-empty report refuses rather than printing success.
    #[tokio::test]
    async fn empty_package_list_errors_instead_of_success() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Update, &[]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("names no package")),
            "{err:?}"
        );
        assert!(!buf.contents().contains("completed"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        // Past the requires_update guard, the empty selection is the error.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "ok");
        seed_package(&mut session);
        let args = matches(&Update, &[]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn no_template_loaded_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Update, &[]);
        let err = Update.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
