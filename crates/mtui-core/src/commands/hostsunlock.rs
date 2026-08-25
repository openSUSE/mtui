//! The `unlock` command (host operation lock / pool claim).

use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::LockOutcome;

use super::support::{add_hosts_arg, host_op_budget};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Unlocks hosts previously locked with `lock`.
///
/// By default removes the
/// zypper/operation lock; `-f`/`--force` also removes locks set by other users
/// or sessions, fanning
/// [`HostsGroup::unlock_force`](mtui_hosts::HostsGroup::unlock_force) out under the
/// [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT) teardown budget, so
/// a dead peer cannot hold the session.
///
/// `-p`/`--pool` removes the host *pool* claim (RRID-based ownership) instead of
/// the zypper/operation lock, fanning [`HostsGroup::pool_unlock`](mtui_hosts::HostsGroup::pool_unlock) out across the
/// active group. With `--force` a claim owned by another template is removed too.
///
/// Like `lock`, host sub-selection via `-t` is not yet honoured for the fan-out
/// (whole active group).
pub struct HostsUnlock;

#[async_trait]
impl Command for HostsUnlock {
    fn name(&self) -> &'static str {
        "unlock"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Unlocks hosts previously locked with `lock`.")
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
                    .help("Force unlock - remove locks set by other users or sessions"),
            )
            .arg(
                Arg::new("pool")
                    .short('p')
                    .long("pool")
                    .action(ArgAction::SetTrue)
                    .help("Remove the pool claim instead of the zypper/operation lock"),
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
        let force = args.get_flag("force");
        if args.get_flag("pool") {
            // Remove the pool claim (RRID-based) instead of the operation lock.
            // `pool_unlock` is best-effort (no per-host outcome map), so confirm
            // the fan-out ran rather than leaving it silent.
            let hosts = session.targets().names().join(", ");
            session.targets_mut().pool_unlock(force).await;
            session
                .display
                .println(&format!("pool claim removed on: {hosts}"));
            return Ok(());
        }

        if force {
            // `unlock_force` reaches foreign locks, which the plain group
            // fan-out (force=false) reports as contended instead. Bounded: this
            // is remote work over a link that may be open locally but dead.
            let names = session.targets().names();
            let budget = host_op_budget();
            // Caller-owned, so the hosts that finished before the budget expired
            // are still attributed once the fan-out future is dropped.
            let collected = std::sync::Mutex::new(BTreeMap::new());
            let timed_out =
                tokio::time::timeout(budget, session.targets_mut().unlock_force(&collected))
                    .await
                    .is_err();
            let outcomes = collected.into_inner().unwrap();
            let failed = report_outcomes(session, &outcomes);

            let stuck: Vec<String> = names
                .into_iter()
                .filter(|n| !outcomes.contains_key(n))
                .collect();
            if timed_out && !stuck.is_empty() {
                let secs = budget.as_secs();
                for name in &stuck {
                    session
                        .display
                        .println(&format!("{name}: unlock not confirmed within {secs}s"));
                }
                return Err(CommandError::Other(format!(
                    "unlock timed out on: {}",
                    stuck.join(", ")
                )));
            }
            return verdict(failed);
        }

        let outcomes = session.targets_mut().unlock().await;
        verdict(report_outcomes(session, &outcomes))
    }
}

/// Prints each host's [`LockOutcome`] and returns the hosts whose release really
/// failed.
///
/// A `Contended` host is a benign foreign lock (skipped without `--force`), not
/// a failure; only a real transport error (`Failed`) counts. Shared by both
/// branches so `--force` reports what happened per host instead of claiming
/// every host was unlocked.
fn report_outcomes(session: &mut Session, outcomes: &BTreeMap<String, LockOutcome>) -> Vec<String> {
    let mut failed: Vec<String> = Vec::new();
    for (host, outcome) in outcomes {
        match outcome {
            LockOutcome::Released => session.display.println(&format!("{host}: unlocked")),
            LockOutcome::Contended => session
                .display
                .println(&format!("{host}: locked by another (use --force)")),
            LockOutcome::Failed(reason) => {
                session
                    .display
                    .println(&format!("{host}: FAILED ({reason})"));
                failed.push(host.clone());
            }
            LockOutcome::Acquired => {}
        }
    }
    failed
}

/// `Ok` unless a host's release really failed.
fn verdict(failed: Vec<String>) -> CommandResult {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(CommandError::Other(format!(
            "unlock failed on: {}",
            failed.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mtui_hosts::{MockConnection, TARGET_LOCK_PATH, Target};
    use mtui_types::enums::TargetState;

    use super::*;
    use crate::commands::testkit;
    use crate::commands::testkit::{matches, session_with_hosts, session_with_targets};

    /// The wire format of a lock owned by somebody else: `timestamp:user:pid`.
    /// Only `--force` may remove it.
    fn foreign_lock(host: &str) -> MockConnection {
        MockConnection::new(host)
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec())
    }

    fn target(host: &str, conn: MockConnection) -> Target {
        Target::with_connection(host, TargetState::Enabled, Box::new(conn))
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(HostsUnlock.name(), "unlock");
        assert_eq!(HostsUnlock.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn unlock_op_lock_succeeds() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &[]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("h1: unlocked"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn unlock_force_succeeds() {
        // The lock-free no-op path: nothing to release still reports released.
        let _budget = testkit::hold_host_op_budget().await;
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-f"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("h1: unlocked"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn force_unlock_clears_foreign_locks_on_all_hosts() {
        // The capability the hand-written serial loop existed for: every host's
        // foreign lock file is really removed, across the fan-out.
        let _budget = testkit::hold_host_op_budget().await;
        let (c1, c2) = (foreign_lock("h1"), foreign_lock("h2"));
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target("h1", c1.clone()), target("h2", c2.clone())],
        );

        let args = matches(&HostsUnlock, &["-f"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();

        assert!(c1.file_contents(TARGET_LOCK_PATH).is_none());
        assert!(c2.file_contents(TARGET_LOCK_PATH).is_none());
        let out = buf.contents();
        assert!(
            out.contains("h1: unlocked") && out.contains("h2: unlocked"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn force_unlock_is_bounded_when_a_host_wedges() {
        // `dead` answers no SFTP at all (a peer gone without a FIN — the link
        // still reports active locally); `healthy` carries a foreign lock. The
        // budget must abandon the first while the second's lock is really
        // force-removed — and `healthy` must be reported as unlocked, not swept
        // into the timeout's host list.
        let _budget = testkit::shrink_host_op_budget(50).await;

        let healthy = foreign_lock("healthy");
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![
                target(
                    "dead",
                    MockConnection::new("dead").with_sftp_session_delay(Duration::from_secs(3600)),
                ),
                target("healthy", healthy.clone()),
            ],
        );

        let args = matches(&HostsUnlock, &["-f"]);
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            HostsUnlock.call(&mut session, &args),
        )
        .await
        .expect("unlock --force must return despite the wedged host")
        .expect_err("a host that never answered must not report success");

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("dead")),
            "{err}"
        );
        assert!(
            !matches!(&err, CommandError::Other(m) if m.contains("healthy")),
            "the host that did release must not be named as timed out: {err}"
        );
        let out = buf.contents();
        assert!(out.contains("dead: unlock not confirmed"), "{out}");
        assert!(out.contains("healthy: unlocked"), "{out}");
        assert!(!out.contains("healthy: unlock not confirmed"), "{out}");
        assert!(
            healthy.file_contents(TARGET_LOCK_PATH).is_none(),
            "the reachable host's foreign lock was still force-removed"
        );
    }

    #[tokio::test]
    async fn pool_unlock_routes_to_pool_branch() {
        // `--pool` fans HostsGroup::pool_unlock out over the group. On an
        // unclaimed host this is a clean no-op; the command must succeed
        // rather than return the old deferred error.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-p"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("pool claim removed on: h1"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn pool_unlock_with_force_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-p", "-f"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
    }

    #[test]
    fn complete_offers_host_names() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert_eq!(
            HostsUnlock.complete(&session, "h", "unlock h"),
            vec!["h1".to_owned()]
        );
    }
}
