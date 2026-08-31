//! The `lock` command (host operation lock).

use std::collections::BTreeSet;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::LockOutcome;

use super::support::{add_hosts_arg, contended_lock_reason, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Locks hosts for exclusive usage (the operation/zypper lock).
///
/// Locks all repository transactions on the target hosts (or only those named
/// with `-t`) with a `timestamp:user:pid[:comment]` remote lock, removed
/// automatically on session exit; a `-c` comment keeps the lock effective
/// against other sessions too.
pub struct HostLock;

#[async_trait]
impl Command for HostLock {
    fn name(&self) -> &'static str {
        "lock"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Locks hosts for exclusive usage (the operation/zypper lock).")
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("comment")
                .short('c')
                .long("comment")
                .num_args(1..)
                .action(ArgAction::Append)
                .value_name("COMMENT")
                .help("Lock comment (keeps the lock effective across sessions)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .targets()
            .names()
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let comment = args
            .get_many::<String>("comment")
            .map(|it| it.cloned().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let session_user = session.config.session_user.clone();
        let targets = session.targets_mut();
        let names: BTreeSet<String> = select_names(targets, args, true)
            .map_err(|e| CommandError::Other(e.to_string()))?
            .into_iter()
            .collect();
        let outcomes = targets.lock_selected(&comment, &names).await;

        // `Contended` is benign — another owner holds the lock — so only a real
        // transport error (`Failed`) fails the command.
        let mut failed: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                LockOutcome::Acquired => session.display.println(&format!("{host}: locked")),
                LockOutcome::Contended(owner) => session.display.println(&format!(
                    "{host}: skipped, {}",
                    contended_lock_reason(owner, &session_user)
                )),
                LockOutcome::Failed(reason) => {
                    session
                        .display
                        .println(&format!("{host}: FAILED ({reason})"));
                    failed.push(host.clone());
                }
                LockOutcome::Released => {}
            }
        }

        if failed.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "lock failed on: {}",
                failed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use mtui_hosts::{MockConnection, TARGET_LOCK_PATH, Target};
    use mtui_types::enums::TargetState;

    use super::*;
    use crate::commands::testkit::{
        matches, session_with_hosts, session_with_lock_outcomes, session_with_targets,
    };

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(HostLock.name(), "lock");
        assert_eq!(HostLock.scope(), Scope::Explicit);
    }

    #[tokio::test]
    async fn lock_without_comment_succeeds() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostLock, &[]);
        HostLock.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("h1: locked"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn lock_failure_errors_and_names_host() {
        let (mut session, buf) =
            session_with_lock_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", false)]);
        let args = matches(&HostLock, &[]);
        let err = HostLock.call(&mut session, &args).await.unwrap_err();
        match err {
            CommandError::Other(msg) => {
                assert!(msg.contains("h2"), "{msg}");
                assert!(!msg.contains("h1"), "only h2 failed: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        let out = buf.contents();
        assert!(out.contains("h1: locked"), "{out}");
        assert!(out.contains("h2: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn lock_names_the_owner_of_a_contended_host() {
        // `lock`'s skip stayed anonymous while `run`/`unlock` learned to name
        // the owner (#521), which would have left it the one contention report
        // in the tree naming nobody. The payload is already in hand.
        let conn = MockConnection::new("h2")
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h2",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
        );
        session.config.session_user = "bob".to_owned();
        let args = matches(&HostLock, &[]);
        HostLock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("h2: skipped, held by alice since Tuesday, 14.11.2023 22:13 UTC"),
            "{out}"
        );
        assert!(
            !out.contains("(you)") && !out.contains("mtui of yours"),
            "a colleague's lock must not read as the caller's own: {out}"
        );
        assert!(
            conn.file_contents(TARGET_LOCK_PATH).is_some(),
            "a contended lock must survive `lock`"
        );
    }

    #[tokio::test]
    async fn lock_scoped_by_t_never_touches_the_unselected_host() {
        // Mutation to catch: reverting to the whole-group `targets.lock(..)`
        // fan-out would also lock h2, and this only fails on the file, not the
        // printed lines.
        let c1 = MockConnection::new("h1");
        let c2 = MockConnection::new("h2");
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(c1.clone())),
                Target::with_connection("h2", TargetState::Enabled, Box::new(c2.clone())),
            ],
        );
        let args = matches(&HostLock, &["-t", "h1"]);
        HostLock.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("h1: locked"), "{}", buf.contents());
        assert!(
            c2.file_contents(TARGET_LOCK_PATH).is_none(),
            "an unselected host must never be locked"
        );
    }

    #[tokio::test]
    async fn lock_named_host_not_connected_errors_and_writes_nothing() {
        let c1 = MockConnection::new("h1");
        let (mut session, _buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(c1.clone()),
            )],
        );
        let args = matches(&HostLock, &["-t", "nosuchhost"]);
        let err = HostLock.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("nosuchhost") && m.contains("not connected")),
            "{err}"
        );
        assert!(c1.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn lock_excludes_a_disabled_host() {
        let c1 = MockConnection::new("h1");
        let c2 = MockConnection::new("h2");
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(c1.clone())),
                Target::with_connection("h2", TargetState::Disabled, Box::new(c2.clone())),
            ],
        );
        let args = matches(&HostLock, &[]);
        HostLock.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("h1: locked"), "{}", buf.contents());
        assert!(
            !buf.contents().contains("h2"),
            "a disabled host must not appear in a bare lock: {}",
            buf.contents()
        );
        assert!(c2.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn lock_with_multiword_comment_joins_it() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // A REMAINDER-style multi-word value, joined with spaces.
        let args = matches(&HostLock, &["-c", "under", "test"]);
        HostLock.call(&mut session, &args).await.unwrap();
    }

    #[test]
    fn complete_offers_host_names() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert_eq!(
            HostLock.complete(&session, "h", "lock h"),
            vec!["h1".to_owned()]
        );
    }
}
