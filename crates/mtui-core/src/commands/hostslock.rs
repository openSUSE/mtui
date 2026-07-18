//! The `lock` command (host operation lock).

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::LockOutcome;

use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Locks hosts for exclusive usage (the operation/zypper lock).
///
/// Ports upstream `mtui.commands.hostslock.HostLock`. Locks all repository
/// transactions on the target hosts with a `timestamp:user:pid[:comment]`
/// remote lock. Enabled locks are removed automatically on session exit; a
/// comment (`-c`) keeps the lock effective against other sessions too.
///
/// `-t` host sub-selection is not yet honoured for the lock fan-out — the whole
/// active group is locked, matching Wave-1 `run`'s group-lock behaviour and the
/// group-merge follow-up (`mtui-rs-qd9`).
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
        Scope::Fanout
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
        let outcomes = session.targets_mut().lock(&comment).await;

        // Report each host's lock verdict. `Contended` is benign (the lock is
        // held by another owner — upstream's `suppress(TargetLockedError)`), so
        // it is *not* a failure; only a real transport error (`Failed`) fails.
        let mut failed: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                LockOutcome::Acquired => session.display.println(&format!("{host}: locked")),
                LockOutcome::Contended => session
                    .display
                    .println(&format!("{host}: already locked (skipped)")),
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
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts, session_with_lock_outcomes};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(HostLock.name(), "lock");
        assert_eq!(HostLock.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn lock_without_comment_succeeds() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostLock, &[]);
        // Best-effort fan-out over mock hosts must not error.
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
    async fn lock_with_multiword_comment_joins_it() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // The comment is joined with spaces; a REMAINDER-style multi-word value.
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
