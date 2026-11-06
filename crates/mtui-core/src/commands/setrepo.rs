//! The `set_repo` command.

use std::collections::BTreeSet;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use mtui_hosts::RepoOp;
use mtui_testreport::reports::warn_on_unlock_failures;

use super::support::{add_hosts_arg, complete_fanout};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Adds or removes an issue repository to or from hosts.
///
/// The required, mutually-exclusive `-A/--add` vs `-R/--remove` picks the
/// operation, fanned out via
/// [`HostsGroup::fanout_set_repo`](mtui_hosts::HostsGroup) and driven by the
/// active report's [`SetRepo`](mtui_hosts::SetRepo) impl. Takes the operation
/// lock first, like `prepare`/`update` do: while another owner holds a host,
/// the command refuses instead of changing that host's repos.
pub struct SetRepo;

#[async_trait]
impl Command for SetRepo {
    fn name(&self) -> &'static str {
        "set_repo"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Adds or removes an issue repository to or from hosts. Takes the operation lock first, \
             like `prepare`/`update`: refuses while another owner holds a host.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("add")
                    .short('A')
                    .long("add")
                    .action(ArgAction::SetTrue)
                    .help("Add issue repos to refhosts"),
            )
            .arg(
                Arg::new("remove")
                    .short('R')
                    .long("remove")
                    .action(ArgAction::SetTrue)
                    .help("Remove issue repos from refhosts"),
            )
            .group(
                ArgGroup::new("operation")
                    .args(["add", "remove"])
                    .required(true),
            )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        // One synonym tuple, so typing any of the four drops all four.
        complete_fanout(
            session,
            &[&["-A", "--add", "-R", "--remove"]],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let operation = if args.get_flag("add") {
            RepoOp::Add
        } else {
            RepoOp::Remove
        };

        let hosts = super::support::hosts_arg(args);
        let names = match &hosts {
            Some(names) if !names.is_empty() && !names.iter().any(|h| h == "all") => {
                Some(names.as_slice())
            }
            _ => None,
        };
        // Split rather than select, so a `-t` subset preserves the unselected
        // hosts in the live report.
        let (mut selected, remainder) = match session.split_targets(names, true) {
            Ok(split) => split,
            Err(e) => return Err(CommandError::Other(e.to_string())),
        };
        if selected.is_empty() {
            session.restore_split_targets(selected, remainder);
            return Err(CommandError::NoRefhostsDefined);
        }
        // SL/PI/OBS can set repos; the null report cannot.
        let has_set_repo = session.metadata().as_set_repo().is_some();
        let result = if has_set_repo {
            // Serialize against other testers' zypper transactions, as
            // prepare/update do: refuse while another owner holds a host
            // rather than changing its repos under them (#665).
            let locked: BTreeSet<String> = selected.names().into_iter().collect();
            if let Err(e) = selected.update_lock_selected(&locked).await {
                session.restore_split_targets(selected, remainder);
                return Err(CommandError::Other(e.to_string()));
            }
            let set_repo = session.metadata().as_set_repo().expect("checked above");
            selected.fanout_set_repo(operation, set_repo).await;
            // `last_repo()` aggregates the whole per-host `zypper ar/rr` +
            // refresh run; any recorded error must fail the command, so an MCP
            // call never silently succeeds on a repo that failed to apply.
            let mut lines: Vec<String> = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            for name in selected.names() {
                let Some(outcome) = selected.get(&name).and_then(mtui_hosts::Target::last_repo)
                else {
                    continue;
                };
                match outcome {
                    Ok(()) => lines.push(format!("{name}: ok")),
                    Err(reason) => {
                        lines.push(format!("{name}: FAILED ({reason})"));
                        failed.push(name);
                    }
                }
            }
            for line in &lines {
                session.display.println(line);
            }
            // Exactly the set locked above: releasing a host never locked
            // would report a failure for a lock nobody took.
            let unlock_outcomes = selected.unlock_selected(&locked).await;
            // Report a stranded lock like prepare/update do.
            warn_on_unlock_failures("set_repo", &unlock_outcomes);
            if failed.is_empty() {
                Ok(())
            } else {
                Err(CommandError::Other(format!(
                    "set_repo failed on: {}",
                    failed.join(", ")
                )))
            }
        } else {
            Err(CommandError::Other("No update loaded".to_owned()))
        };
        session.restore_split_targets(selected, remainder);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{
        empty_session, log_capture, matches, session_with_hosts, session_with_setrepo_outcomes,
        session_with_targets_and_set_repo,
    };
    use mtui_hosts::{MockConnection, MockSftpOp, TARGET_LOCK_PATH, Target};
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;
    use std::path::{Path, PathBuf};

    #[test]
    fn complete_offers_addremove_group_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = SetRepo.complete(&session, "", "set_repo ");
        for f in ["-t", "-A", "--add", "-R", "--remove"] {
            assert!(out.contains(&f.to_owned()), "missing {f}: {out:?}");
        }
        assert!(out.contains(&"h1".to_owned()), "{out:?}");
        let after = SetRepo.complete(&session, "-", "set_repo -A ");
        assert!(
            !after
                .iter()
                .any(|c| ["-A", "--add", "-R", "--remove"].contains(&c.as_str()))
        );
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(SetRepo.name(), "set_repo");
        assert_eq!(SetRepo.scope(), Scope::Explicit);
    }

    #[test]
    fn operation_group_is_required() {
        let base = clap::Command::new("set_repo").no_binary_name(true);
        assert!(
            SetRepo
                .configure(base)
                .try_get_matches_from([""; 0])
                .is_err()
        );
    }

    #[test]
    fn add_and_remove_are_mutually_exclusive() {
        let base = clap::Command::new("set_repo").no_binary_name(true);
        assert!(
            SetRepo
                .configure(base)
                .try_get_matches_from(["-A", "-R"])
                .is_err()
        );
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn report_without_set_repo_capability_errors() {
        // FakeReport's `as_set_repo` is None, as the null/unloaded report's is.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m == "No update loaded"));
        // The group is restored even on the capability-miss path.
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn all_hosts_succeed_prints_ok_summary() {
        let (mut session, buf) =
            session_with_setrepo_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", true)]);
        let args = matches(&SetRepo, &["-A"]);
        SetRepo.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1: ok"), "{out}");
        assert!(out.contains("h2: ok"), "{out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    #[tokio::test]
    async fn one_host_fails_errors_and_reports_both() {
        let (mut session, buf) =
            session_with_setrepo_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", false)]);
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        match err {
            CommandError::Other(msg) => {
                assert!(msg.contains("h2"), "{msg}");
                assert!(!msg.contains("h1"), "only h2 failed: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        let out = buf.contents();
        assert!(out.contains("h1: ok"), "{out}");
        assert!(out.contains("h2: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn set_repo_refuses_when_a_host_is_locked_by_another_owner() {
        // h2 carries a foreign operation lock: the command must refuse
        // without running any repo change on either host.
        let h1conn = MockConnection::new("h1").with_default(CommandLog::new("", "", "", 0, 0));
        let h1handle = h1conn.clone();
        let h2conn = MockConnection::new("h2")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_file(TARGET_LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        let (mut session, _buf) = session_with_targets_and_set_repo(
            "SUSE:Maintenance:1:1",
            vec![
                Target::with_connection("h1", TargetState::Enabled, Box::new(h1conn)),
                Target::with_connection("h2", TargetState::Enabled, Box::new(h2conn)),
            ],
        );
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        match err {
            CommandError::Other(msg) => assert!(msg.contains("h2"), "{msg}"),
            other => panic!("expected a lock refusal, got {other:?}"),
        }
        assert!(
            h1handle.commands().is_empty(),
            "refusal must precede any repo change"
        );
        // h1's lockfile write proves its lock was taken before the abort;
        // its absence now proves `update_lock_where` rolled the partial take
        // back instead of stranding it on the refusal path.
        assert!(
            h1handle.sftp_ops().iter().any(|op| {
                matches!(op, MockSftpOp::Write { path, exclusive }
                if path == Path::new(TARGET_LOCK_PATH) && *exclusive)
            }),
            "h1 must have taken the lock before the abort"
        );
        assert!(
            h1handle.file_contents(TARGET_LOCK_PATH).is_none(),
            "refusal must roll back h1's partial take"
        );
        // The split group is restored even on the lock-refusal path.
        assert_eq!(
            session.targets().names(),
            vec!["h1".to_owned(), "h2".to_owned()]
        );
    }

    #[tokio::test]
    async fn set_repo_takes_and_releases_the_operation_lock() {
        let conn = MockConnection::new("h1").with_default(CommandLog::new("", "", "", 0, 0));
        let handle = conn.clone();
        let (mut session, _buf) = session_with_targets_and_set_repo(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn),
            )],
        );
        let args = matches(&SetRepo, &["-A"]);
        SetRepo.call(&mut session, &args).await.unwrap();
        // The lockfile write in the SFTP op log proves the lock was taken:
        // without the `update_lock_selected` call above there is no write
        // and this fails. The shell `issued` log cannot pin that ordering —
        // it records no SFTP ops — so presence on both logs is the oracle.
        let ops = handle.sftp_ops();
        let take = ops
            .iter()
            .position(|op| {
                matches!(op, MockSftpOp::Write { path, exclusive }
                if path == Path::new(TARGET_LOCK_PATH) && *exclusive)
            })
            .expect("taking the lock must write the lockfile");
        assert!(
            handle.commands().iter().any(|c| c == "zypper -n ref"),
            "the repo change must run while the lock is held; got {:?}",
            handle.commands()
        );
        let release = ops
            .iter()
            .position(|op| op == &MockSftpOp::Remove(PathBuf::from(TARGET_LOCK_PATH)))
            .expect("releasing the lock must remove the lockfile");
        assert!(
            take < release,
            "the lockfile write must precede its removal"
        );
        // A stranded lock would still read back here.
        assert!(
            !session
                .targets_mut()
                .get_mut("h1")
                .unwrap()
                .is_locked()
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn set_repo_warns_when_the_lock_does_not_release() {
        // The repo change succeeds but the unlock's remove fails, so the
        // outcome map carries a `Failed` the command must not swallow.
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .failing_sftp_remove();
        let (mut session, _buf) = session_with_targets_and_set_repo(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn),
            )],
        );
        let args = matches(&SetRepo, &["-A"]);
        log_capture::start();
        SetRepo
            .call(&mut session, &args)
            .await
            .expect("the repo change itself succeeds");
        let logged = log_capture::take();
        assert!(
            logged.iter().any(|c| c.message.as_deref().is_some_and(|m| m
                .contains("the set_repo operation lock did not release")
                && m.contains("h1"))),
            "a stranded lock must warn naming the op and host; got: {:?}",
            logged
                .iter()
                .filter_map(|c| c.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn set_repo_releases_the_lock_when_the_repo_fails() {
        // The repo fan-out fails, so the command errors — but the lock taken
        // for it must not be stranded.
        let conn = MockConnection::new("h1").with_default(CommandLog::new(
            "zypper -n ref",
            "",
            "refresh failed",
            1,
            0,
        ));
        let (mut session, _buf) = session_with_targets_and_set_repo(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn),
            )],
        );
        let args = matches(&SetRepo, &["-A"]);
        SetRepo.call(&mut session, &args).await.unwrap_err();
        assert!(
            !session
                .targets_mut()
                .get_mut("h1")
                .unwrap()
                .is_locked()
                .await
                .unwrap(),
            "a failed set_repo must not strand its lock"
        );
    }

    #[tokio::test]
    async fn set_repo_t_subset_keeps_unselected_host() {
        // The split+merge must preserve h2 even when the op itself no-ops on the
        // capability miss.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&SetRepo, &["-A", "-t", "h1"]);
        let _ = SetRepo.call(&mut session, &args).await;
        assert_eq!(
            session.targets().names(),
            vec!["h1".to_owned(), "h2".to_owned()]
        );
    }
}
