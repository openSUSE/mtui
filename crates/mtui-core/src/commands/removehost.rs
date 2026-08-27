//! The `remove_host` command.

use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, host_op_budget, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Disconnects from a host and removes it from the list.
///
/// The selected hosts move into their own group and are
/// [`close`](mtui_hosts::HostsGroup::close)d there (dropping the remote
/// operation and pool-claim lock files) concurrently, under one shared
/// [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT) budget for the
/// whole call. Each in-process arbiter claim is then released via
/// [`TestReport::release_pool_claim`](mtui_testreport::TestReport::release_pool_claim),
/// *outside* the budget so an abandoned teardown never skips it — otherwise a
/// scarce pool host stays busy in the process-global
/// [`HostArbiter`](mtui_hosts::HostArbiter) for the life of an MCP session. Only
/// hosts the budget cut short are named. With no `-t` every host is removed.
pub struct RemoveHost;

#[async_trait]
impl Command for RemoveHost {
    fn name(&self) -> &'static str {
        "remove_host"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Disconnects from a host and removes it from the list.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
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
        // enabled=false: remove disabled hosts too.
        let hosts = select_names(session.targets_mut(), args, false)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        // Dropping a target alone never runs `close`, so the doomed hosts need
        // their own group to be torn down in — concurrently. The `Err` is
        // unreachable, `select_names` having validated membership already.
        let (mut doomed, rest) = session
            .split_targets(Some(&hosts), false)
            .map_err(|e| CommandError::Other(e.to_string()))?;

        // Best-effort: the target is being dropped anyway, so the budget only
        // bounds how long a dead peer can hold the session.
        let budget = host_op_budget();
        // Caller-owned, so hosts that finished before the budget expired are
        // still attributed once the fan-out future is dropped.
        let collected = std::sync::Mutex::new(BTreeMap::new());
        let timed_out = tokio::time::timeout(budget, doomed.close_collecting(None, &collected))
            .await
            .is_err();
        let outcomes = collected.into_inner().unwrap();
        for (host, outcome) in &outcomes {
            if let Err(e) = outcome {
                tracing::warn!("failed to disconnect from {host}: {e}");
            }
        }
        // Only the cut-short hosts; blaming the whole call would name hosts
        // whose close already completed.
        if timed_out {
            let secs = budget.as_secs();
            for host in hosts.iter().filter(|h| !outcomes.contains_key(*h)) {
                session.display.println(&format!(
                    "still disconnecting from {host} after {secs} seconds; abandoning"
                ));
            }
        }
        drop(doomed);
        *session.targets_mut() = rest;

        // Outside the budget: an in-process claim must be released even when the
        // remote teardown was abandoned. Also prunes slot candidates.
        for name in &hosts {
            session.metadata_mut().release_pool_claim(name);
        }
        session
            .display
            .println(&format!("Removed {}", hosts.join(", ")));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::TargetState;

    use super::*;
    use crate::commands::testkit;
    use crate::commands::testkit::{matches, session_with_hosts, session_with_targets};

    /// A target on a mock that stays *locally* active — the shape of a peer that
    /// vanished without closing its SSH link.
    fn target_with(host: &str, conn: MockConnection) -> Target {
        Target::with_connection(host, TargetState::Enabled, Box::new(conn))
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(RemoveHost.name(), "remove_host");
        assert_eq!(RemoveHost.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn removes_named_host() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert!(!session.targets().contains("h1"));
        assert!(session.targets().contains("h2"));
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("Removed h1"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn removes_all_when_no_target() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&RemoveHost, &[]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert!(session.targets().is_empty());
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&RemoveHost, &["-t", "ghost"]);
        let err = RemoveHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn removed_pool_host_is_reacquirable_in_process() {
        use mtui_hosts::{HostArbiter, Owner};

        // Leaked to the `&'static` the report field needs, without touching the
        // process-global singleton.
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        {
            let base = session.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
            base.pool_claims.insert("h1".to_owned());
            base.slot_candidates
                .insert("slot0".to_owned(), vec!["h1".to_owned()]);
        }
        assert!(arbiter.try_acquire("h1", &owner));
        // A foreign owner cannot take it while we hold the claim.
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(!arbiter.try_acquire("h1", &other));

        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();

        // A sibling session can re-acquire the freed pool host.
        assert!(!session.targets().contains("h1"));
        assert!(arbiter.try_acquire("h1", &other));
        assert!(!session.metadata().base().pool_claims.contains("h1"));
    }

    #[tokio::test]
    async fn remove_host_is_bounded_when_a_close_wedges() {
        // The healthy sibling must still really be torn down, which a bound that
        // skipped the work would break.
        let gate = Arc::new(tokio::sync::Notify::new());
        let good = MockConnection::new("good");
        let targets = vec![
            target_with("good", good.clone()),
            target_with(
                "dead",
                MockConnection::new("dead").with_blocking_close(Arc::clone(&gate)),
            ),
        ];
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", targets);

        let args = matches(&RemoveHost, &[]);
        testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(Duration::from_secs(5), RemoveHost.call(&mut session, &args))
                .await
                .expect("remove_host must return despite the wedged host")
                .expect("remove_host ok")
        })
        .await;

        assert!(session.targets().is_empty());
        assert!(good.is_closed(), "the healthy host was really torn down");
        assert!(buf.contents().contains("Removed"), "{}", buf.contents());
        // `good`'s close really lands in the outcomes map inside the 50ms
        // budget, so warning for every host in the call fails here.
        assert!(
            buf.contents().contains("still disconnecting from dead"),
            "{}",
            buf.contents()
        );
        assert!(
            !buf.contents().contains("still disconnecting from good"),
            "{}",
            buf.contents()
        );

        gate.notify_waiters();
    }

    #[tokio::test(start_paused = true)]
    async fn remove_host_closes_hosts_concurrently() {
        // Three 10s teardowns under the production 45s budget: ~10s concurrent
        // against 30s serial. Virtual time, so the wall cost is nil, and the
        // budget override is task-local so no sibling test's can reach here.
        let slow = |h: &str| MockConnection::new(h).with_close_delay(Duration::from_secs(10));
        let (c1, c2, c3) = (slow("h1"), slow("h2"), slow("h3"));
        let targets = vec![
            target_with("h1", c1.clone()),
            target_with("h2", c2.clone()),
            target_with("h3", c3.clone()),
        ];
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", targets);

        let args = matches(&RemoveHost, &[]);
        let start = tokio::time::Instant::now();
        RemoveHost.call(&mut session, &args).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(20),
            "closes must overlap (serial would be 30s), took {elapsed:?}"
        );
        assert!(c1.is_closed() && c2.is_closed() && c3.is_closed());
        assert!(session.targets().is_empty());
    }

    #[tokio::test]
    async fn abandoned_close_still_releases_pool_claim() {
        use mtui_hosts::{HostArbiter, Owner};

        // The arbiter release is the one part of teardown that always works;
        // inside the budget, losing it strands a scarce pool host for the
        // session's life.
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let gate = Arc::new(tokio::sync::Notify::new());
        let (mut session, _buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target_with(
                "dead",
                MockConnection::new("dead").with_blocking_close(Arc::clone(&gate)),
            )],
        );
        {
            let base = session.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner.clone());
            base.pool_claims.insert("dead".to_owned());
        }
        assert!(arbiter.try_acquire("dead", &owner));
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        assert!(!arbiter.try_acquire("dead", &other));

        let args = matches(&RemoveHost, &[]);
        testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(Duration::from_secs(5), RemoveHost.call(&mut session, &args))
                .await
                .expect("remove_host must return despite the wedged host")
                .expect("remove_host ok")
        })
        .await;

        assert!(
            arbiter.try_acquire("dead", &other),
            "the claim is released even though the remote teardown was abandoned"
        );
        assert!(!session.metadata().base().pool_claims.contains("dead"));

        gate.notify_waiters();
    }

    #[tokio::test]
    async fn close_error_is_warned_not_timed_out() {
        // An erroring close lands in the outcomes map with an `Err`, driving the
        // `Ok(outcomes)` arm's warn — distinct from the abandonment arm, which
        // never sees the host's outcome at all.
        testkit::log_capture::start();
        let (mut session, _buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target_with(
                "flaky",
                MockConnection::new("flaky").with_failing_close(),
            )],
        );

        let args = matches(&RemoveHost, &[]);
        RemoveHost.call(&mut session, &args).await.unwrap();

        let logged = testkit::log_capture::take();
        assert!(
            logged.iter().any(|c| c
                .message
                .as_deref()
                .is_some_and(|m| m.contains("failed to disconnect from flaky"))),
            "expected a 'failed to disconnect from flaky' warning, got {:?}",
            logged
                .iter()
                .filter_map(|c| c.message.clone())
                .collect::<Vec<_>>()
        );
        assert!(session.targets().is_empty());
    }

    #[test]
    fn complete_offers_host_names() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert_eq!(
            RemoveHost.complete(&session, "h", "remove_host h"),
            vec!["h1".to_owned()]
        );
    }
}
