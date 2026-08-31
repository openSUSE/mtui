//! The `unlock` command (host operation lock / pool claim).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::{LockOutcome, LockOwner};

use super::support::{add_hosts_arg, contended_lock_reason, host_op_budget, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Unlocks hosts previously locked with `lock`.
///
/// By default removes the zypper/operation lock on the target hosts (or only
/// those named with `-t`); `-f`/`--force` also removes locks set by other
/// users or sessions, fanning
/// [`HostsGroup::unlock_force`](mtui_hosts::HostsGroup::unlock_force) out under
/// the [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT) teardown
/// budget so a dead peer cannot hold the session.
///
/// `-p`/`--pool` removes the host *pool* claim (RRID-based ownership) instead,
/// fanning
/// [`HostsGroup::pool_unlock_collecting`](mtui_hosts::HostsGroup::pool_unlock_collecting)
/// out under the same budget; with `--force` a claim owned by another template
/// goes too.
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
        Scope::Explicit
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
        let targets = session.targets_mut();
        let names =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        let selected: BTreeSet<String> = names.iter().cloned().collect();

        if args.get_flag("pool") {
            // Reaches the same possibly-dead links as `unlock_force`, so it gets
            // the same budget and per-host attribution.
            let budget = host_op_budget();
            let collected = std::sync::Mutex::new(BTreeMap::new());
            let timed_out = tokio::time::timeout(
                budget,
                session
                    .targets_mut()
                    .pool_unlock_collecting(force, &selected, &collected),
            )
            .await
            .is_err();
            let outcomes = collected.into_inner().unwrap();
            return bounded_unlock(
                session,
                budget,
                UnlockKind::Pool,
                names,
                timed_out,
                outcomes,
            );
        }

        if force {
            // `unlock_force` reaches foreign locks, which the plain fan-out
            // reports as contended instead. Bounded: remote work over a link that
            // may be open locally but dead.
            let budget = host_op_budget();
            // Caller-owned, so hosts that finished before the budget expired are
            // still attributed once the fan-out future is dropped.
            let collected = std::sync::Mutex::new(BTreeMap::new());
            let timed_out = tokio::time::timeout(
                budget,
                session.targets_mut().unlock_force(&selected, &collected),
            )
            .await
            .is_err();
            let outcomes = collected.into_inner().unwrap();
            return bounded_unlock(
                session,
                budget,
                UnlockKind::Force,
                names,
                timed_out,
                outcomes,
            );
        }

        // `Force` picks labels only; `--force` never emits `Contended`
        // (`TargetLock::unlock`'s only contended arm is `&& !force`), so that
        // variant's contended line is plain unlock's alone.
        let outcomes = session.targets_mut().unlock_selected(&selected).await;
        verdict(
            &UnlockKind::Force,
            report_outcomes(session, &UnlockKind::Force, &outcomes),
        )
    }
}

/// Distinguishes `--force`'s and `--pool`'s bounded fan-outs in the messages
/// [`bounded_unlock`] prints; the fan-out call itself stays at each call site,
/// `unlock_force` and `pool_unlock_collecting` being different methods.
enum UnlockKind {
    Force,
    Pool,
}

impl UnlockKind {
    /// The per-stuck-host line's verb phrase, e.g. `"{host}: {phrase} within
    /// {secs}s"`.
    fn not_confirmed(&self) -> &'static str {
        match self {
            Self::Force => "unlock not confirmed",
            Self::Pool => "pool claim release not confirmed",
        }
    }

    /// The `CommandError`'s leading clause, e.g. `"{clause}: {stuck hosts}"`.
    fn timed_out_on(&self) -> &'static str {
        match self {
            Self::Force => "unlock timed out on",
            Self::Pool => "pool unlock timed out on",
        }
    }

    /// [`LockOutcome::Released`]'s per-host line, e.g. `"{host}: {label}"`.
    fn released_label(&self) -> &'static str {
        match self {
            Self::Force => "unlocked",
            Self::Pool => "pool claim removed",
        }
    }

    /// [`LockOutcome::Contended`]'s per-host line, e.g. `"{host}: {label}"`,
    /// naming the owner the fan-out carried out of the refused release.
    fn contended_label(&self, owner: &LockOwner, session_user: &str) -> String {
        match self {
            Self::Force => contended_lock_reason(owner, session_user),
            // Pool ownership is RRID-based, so the owning *user* matching says
            // nothing about which process holds the claim: no own/foreign split
            // to draw, only the same list_locks-first steer and the same scope
            // note, since `--pool --force` is whole-group too.
            Self::Pool if owner.by.is_empty() => {
                "pool claim held by an unknown owner; check list_locks \
                 (unlock --pool --force clears every selected host)"
                    .to_owned()
            }
            Self::Pool => format!(
                "pool claim held by {} since {}; check list_locks \
                 (unlock --pool --force clears every selected host)",
                owner.by, owner.since
            ),
        }
    }

    /// [`verdict`]'s leading clause when a host's release really failed.
    fn failed_on_label(&self) -> &'static str {
        match self {
            Self::Force => "unlock failed on",
            Self::Pool => "pool unlock failed on",
        }
    }
}

/// Reports `outcomes` and returns the budget's verdict, naming only the hosts it
/// cut short. Shared tail of the `--force` and `--pool` branches: both fan out
/// under [`host_op_budget`] into a caller-owned map, so a host abandoned
/// mid-flight is simply absent from `outcomes` and `stuck` is exactly that
/// absence — kept distinct from `failed` (answered, but the release errored).
fn bounded_unlock(
    session: &mut Session,
    budget: std::time::Duration,
    kind: UnlockKind,
    names: Vec<String>,
    timed_out: bool,
    outcomes: BTreeMap<String, LockOutcome>,
) -> CommandResult {
    let failed = report_outcomes(session, &kind, &outcomes);

    let stuck: Vec<String> = names
        .into_iter()
        .filter(|n| !outcomes.contains_key(n))
        .collect();
    if timed_out && !stuck.is_empty() {
        let secs = budget.as_secs();
        let not_confirmed = kind.not_confirmed();
        for name in &stuck {
            session
                .display
                .println(&format!("{name}: {not_confirmed} within {secs}s"));
        }
        let mut msg = format!("{}: {}", kind.timed_out_on(), stuck.join(", "));
        if !failed.is_empty() {
            msg.push_str(&format!("; failed on: {}", failed.join(", ")));
        }
        return Err(CommandError::Other(msg));
    }
    verdict(&kind, failed)
}

/// Prints each host's [`LockOutcome`] and returns those whose release really
/// failed. A `Contended` host is a benign foreign lock (skipped without
/// `--force`), not a failure; only a real transport error (`Failed`) counts.
/// `kind` picks the `Released`/`Contended` wording so an unlocked host and one
/// whose pool claim was removed are never reported in each other's words.
fn report_outcomes(
    session: &mut Session,
    kind: &UnlockKind,
    outcomes: &BTreeMap<String, LockOutcome>,
) -> Vec<String> {
    let session_user = session.config.session_user.clone();
    let mut failed: Vec<String> = Vec::new();
    for (host, outcome) in outcomes {
        match outcome {
            LockOutcome::Released => session
                .display
                .println(&format!("{host}: {}", kind.released_label())),
            LockOutcome::Contended(owner) => session.display.println(&format!(
                "{host}: {}",
                kind.contended_label(owner, &session_user)
            )),
            LockOutcome::Failed(reason) => {
                session
                    .display
                    .println(&format!("{host}: FAILED ({reason})"));
                failed.push(host.clone());
            }
            LockOutcome::Skipped(reason) => session
                .display
                .println(&format!("{host}: skipped, {reason}")),
            LockOutcome::Acquired => {}
        }
    }
    failed
}

/// `Ok` unless a host's release really failed.
fn verdict(kind: &UnlockKind, failed: Vec<String>) -> CommandResult {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(CommandError::Other(format!(
            "{}: {}",
            kind.failed_on_label(),
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
        assert_eq!(HostsUnlock.scope(), Scope::Explicit);
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
    async fn unlock_reports_an_unconnected_host_as_skipped_not_unlocked() {
        // Mutation to catch: dropping `unlock_where`'s `has_operation_lock`
        // guard makes an unconnected target print `unlocked` again
        // (`Target::unlock_reporting` is a no-op `Ok(())` on `self.lock ==
        // None`).
        let unconnected = Target::new(&mtui_config::Config::default(), "h1", TargetState::Enabled);
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![unconnected]);
        let args = matches(&HostsUnlock, &[]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1: skipped, not connected"), "{out}");
        assert!(!out.contains("h1: unlocked"), "{out}");
    }

    #[tokio::test]
    async fn force_unlock_reports_an_unconnected_host_as_skipped_not_unlocked() {
        let unconnected = Target::new(&mtui_config::Config::default(), "h1", TargetState::Enabled);
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![unconnected]);
        let args = matches(&HostsUnlock, &["-f"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1: skipped, not connected"), "{out}");
        assert!(!out.contains("h1: unlocked"), "{out}");
    }

    #[tokio::test]
    async fn pool_unlock_reports_an_unconnected_host_as_skipped_not_removed() {
        let unconnected = Target::new(&mtui_config::Config::default(), "h1", TargetState::Enabled);
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![unconnected]);
        let args = matches(&HostsUnlock, &["-p"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1: skipped, not connected"), "{out}");
        assert!(!out.contains("h1: pool claim removed"), "{out}");
    }

    #[tokio::test]
    async fn unlock_reports_a_foreign_lock_as_contended_without_force() {
        // Benign contention: left in place, and not counted as a failure.
        let foreign = foreign_lock("h1");
        let (mut session, buf) =
            session_with_targets("SUSE:Maintenance:1:1", vec![target("h1", foreign.clone())]);
        session.config.session_user = "bob".to_owned();
        let args = matches(&HostsUnlock, &[]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains(
                "h1: held by otheruser since Tuesday, 14.11.2023 22:13 UTC, possibly a live \
                 mtui; check list_locks (unlock --force clears every selected host)"
            ),
            "{out}"
        );
        assert!(
            !out.contains("(you)") && !out.contains("mtui of yours"),
            "{out}"
        );
        assert!(foreign.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn unlock_names_the_caller_as_the_owner_of_their_stranded_lock() {
        // Same user, foreign PID: `is_mine` still says no (the PID check is what
        // serialises one tester's concurrent zypper transactions), so the lock
        // survives — but the report names the caller instead of an anonymous
        // "another", hedges (that signature is a live sibling mtui just as
        // readily as a strand) and asks for `list_locks` first (#521).
        let me = mtui_config::Config::default().session_user;
        let line = format!("1700000000:{me}:{}", std::process::id() + 1).into_bytes();
        let conn = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, line.clone());
        let (mut session, buf) =
            session_with_targets("SUSE:Maintenance:1:1", vec![target("h1", conn.clone())]);
        let args = matches(&HostsUnlock, &[]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains(&format!("h1: held by {me} (you)")), "{out}");
        assert!(out.contains("possibly another mtui of yours"), "{out}");
        assert!(
            out.contains("check list_locks and your other sessions"),
            "{out}"
        );
        assert!(
            out.contains("unlock --force clears every selected host"),
            "{out}"
        );
        assert!(!out.contains("possibly a live mtui"), "{out}");
        assert_eq!(conn.file_contents(TARGET_LOCK_PATH), Some(line));
    }

    /// The wire format of an operation lock this test process itself holds:
    /// `TargetLock::is_mine` matches on user *and* pid, unlike the pool claim's
    /// RRID-based check (user + RRID, pid ignored; user-only with no RRID).
    fn own_op_lock() -> Vec<u8> {
        let me = mtui_config::Config::default().session_user;
        let pid = std::process::id();
        format!("1700000000:{me}:{pid}").into_bytes()
    }

    #[tokio::test]
    async fn unlock_scoped_by_t_leaves_the_unselected_hosts_lock_intact() {
        // The reporter's verbatim repro: `unlock -t h2` with both h1 and h2
        // locked by this session must leave h1's lock file byte-identical.
        // Mutation to catch: reverting to the whole-group `targets.unlock()`
        // fan-out would also unlock h1.
        let (line1, line2) = (own_op_lock(), own_op_lock());
        let c1 = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, line1.clone());
        let c2 = MockConnection::new("h2").with_file(TARGET_LOCK_PATH, line2);
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target("h1", c1.clone()), target("h2", c2.clone())],
        );
        let args = matches(&HostsUnlock, &["-t", "h2"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("h2: unlocked"),
            "{}",
            buf.contents()
        );
        assert_eq!(
            c1.file_contents(TARGET_LOCK_PATH),
            Some(line1),
            "unselected h1's lock must survive"
        );
        assert!(c2.file_contents(TARGET_LOCK_PATH).is_none());
    }

    #[tokio::test]
    async fn force_unlock_scoped_by_t_leaves_the_unselected_hosts_foreign_lock() {
        let (c1, c2) = (foreign_lock("h1"), foreign_lock("h2"));
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target("h1", c1.clone()), target("h2", c2.clone())],
        );
        let args = matches(&HostsUnlock, &["-f", "-t", "h1"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("h1: unlocked"),
            "{}",
            buf.contents()
        );
        assert!(c1.file_contents(TARGET_LOCK_PATH).is_none());
        assert!(
            c2.file_contents(TARGET_LOCK_PATH).is_some(),
            "an unselected host's foreign lock must survive --force"
        );
    }

    #[tokio::test]
    async fn unlock_named_host_not_connected_errors() {
        let c1 = MockConnection::new("h1").with_file(TARGET_LOCK_PATH, own_op_lock());
        let (mut session, _buf) =
            session_with_targets("SUSE:Maintenance:1:1", vec![target("h1", c1.clone())]);
        let args = matches(&HostsUnlock, &["-t", "nosuchhost"]);
        let err = HostsUnlock.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("nosuchhost") && m.contains("not connected")),
            "{err}"
        );
        assert!(c1.file_contents(TARGET_LOCK_PATH).is_some());
    }

    #[tokio::test]
    async fn unlock_reports_a_real_failure_without_timing_out() {
        // A removal that errors for real (not "already gone") must propagate as
        // `Failed`, not `Contended`. No wedge involved.
        let broken = MockConnection::new("broken")
            .with_file(TARGET_LOCK_PATH, own_op_lock())
            .failing_sftp_remove();
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target("broken", broken.clone())],
        );
        let args = matches(&HostsUnlock, &[]);
        let err = HostsUnlock.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("unlock failed on: broken")),
            "{err}"
        );
        assert!(
            buf.contents().contains("broken: FAILED"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn force_unlock_clears_foreign_locks_on_all_hosts() {
        // Every host's foreign lock file is really removed, across the fan-out.
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
        // `dead` answers no SFTP (a peer gone without a FIN, so the link still
        // reports active locally). The budget must abandon it while `healthy`'s
        // foreign lock is really force-removed and reported as unlocked, not
        // swept into the timeout's host list.
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
        let err = testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                Duration::from_secs(5),
                HostsUnlock.call(&mut session, &args),
            )
            .await
            .expect("unlock --force must return despite the wedged host")
            .expect_err("a host that never answered must not report success")
        })
        .await;

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
    async fn force_unlock_timeout_still_names_a_failed_host() {
        // `dead` wedges, `broken`'s remove genuinely fails, `healthy` releases:
        // the error must name the first two and never the third.
        let broken = MockConnection::new("broken")
            .with_file(TARGET_LOCK_PATH, b"1700000000:otheruser:99999".to_vec())
            .failing_sftp_remove();
        let healthy = foreign_lock("healthy");
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![
                target(
                    "dead",
                    MockConnection::new("dead").with_sftp_session_delay(Duration::from_secs(3600)),
                ),
                target("broken", broken.clone()),
                target("healthy", healthy.clone()),
            ],
        );

        let args = matches(&HostsUnlock, &["-f"]);
        let err = testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                Duration::from_secs(5),
                HostsUnlock.call(&mut session, &args),
            )
            .await
            .expect("unlock --force must return despite the wedged host")
            .expect_err("a wedged and a genuinely failed host must not report success")
        })
        .await;

        let CommandError::Other(msg) = &err else {
            panic!("expected CommandError::Other, got {err}");
        };
        assert!(msg.contains("dead"), "{msg}");
        assert!(msg.contains("broken"), "{msg}");
        assert!(!msg.contains("healthy"), "{msg}");
        let out = buf.contents();
        assert!(out.contains("broken: FAILED"), "{out}");
        assert!(out.contains("healthy: unlocked"), "{out}");
    }

    #[tokio::test]
    async fn pool_unlock_routes_to_pool_branch() {
        // On an unclaimed host this is a clean no-op, but the release must
        // still be attributed per host, not as one whole-group line.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-p"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("h1: pool claim removed"),
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

    /// Wire path of the pool-claim lock file; `mtui_hosts`'s own constant is
    /// `pub(crate)` and unreachable here, as with `/var/lock/mtui.lock` in
    /// `commands::run`'s tests.
    const POOL_LOCK_PATH: &str = "/var/lock/mtui-pool.lock";

    /// A pool claim this session's identity owns, in the wire format
    /// `timestamp:user:pid:mtui pool <rrid> [<owner>]`. The built target still
    /// needs `Target::set_rrid` for `PoolLock::is_mine` to recognise it.
    fn own_pool_claim(rrid: &str) -> Vec<u8> {
        let me = mtui_config::Config::default().session_user;
        format!("1700000000:{me}:1:mtui pool {rrid} [{rrid}]").into_bytes()
    }

    /// A claim under a different template's RRID: `PoolLock::is_mine` is
    /// RRID-based, so this is foreign whoever stamped it.
    fn foreign_pool_claim() -> Vec<u8> {
        b"1700000000:alice:4242:mtui pool SUSE:Maintenance:9:9 [alice]".to_vec()
    }

    #[tokio::test]
    async fn pool_unlock_scoped_by_t_leaves_the_unselected_hosts_claim() {
        let rrid = "SUSE:Maintenance:1:1";
        let c1 = MockConnection::new("h1").with_file(POOL_LOCK_PATH, own_pool_claim(rrid));
        let c2 = MockConnection::new("h2").with_file(POOL_LOCK_PATH, own_pool_claim(rrid));
        let mut h1 = target("h1", c1.clone());
        h1.set_rrid(rrid);
        let mut h2 = target("h2", c2.clone());
        h2.set_rrid(rrid);
        let (mut session, buf) = session_with_targets(rrid, vec![h1, h2]);

        let args = matches(&HostsUnlock, &["-p", "-t", "h1"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents().contains("h1: pool claim removed"),
            "{}",
            buf.contents()
        );
        assert!(c1.file_contents(POOL_LOCK_PATH).is_none());
        assert!(
            c2.file_contents(POOL_LOCK_PATH).is_some(),
            "an unselected host's pool claim must survive"
        );
    }

    #[tokio::test]
    async fn pool_unlock_reports_a_foreign_claim_as_contended_without_force() {
        let rrid = "SUSE:Maintenance:1:1";
        let conn = MockConnection::new("h1").with_file(POOL_LOCK_PATH, foreign_pool_claim());
        let mut h1 = target("h1", conn.clone());
        h1.set_rrid(rrid);
        let (mut session, buf) = session_with_targets(rrid, vec![h1]);

        let args = matches(&HostsUnlock, &["-p"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents().contains(
                "h1: pool claim held by alice since Tuesday, 14.11.2023 22:13 UTC; check \
                 list_locks (unlock --pool --force clears every selected host)"
            ),
            "{}",
            buf.contents()
        );
        assert!(conn.file_contents(POOL_LOCK_PATH).is_some());
    }

    /// The pool claim's unnamed-owner fallback (the claim line was never read).
    /// Uncovered before #521: with no owner to name it draws no inference to
    /// hedge, so all it owes is the `list_locks`-first steer and `--force`'s
    /// whole-group scope.
    #[test]
    fn pool_contended_label_without_an_owner_names_it_and_scopes_the_force_remedy() {
        let line = UnlockKind::Pool.contended_label(&LockOwner::default(), "bob");
        assert!(
            line.contains("pool claim held by an unknown owner"),
            "{line}"
        );
        assert!(line.contains("list_locks"), "{line}");
        assert!(
            line.contains("unlock --pool --force clears every selected host"),
            "{line}"
        );
    }

    #[tokio::test]
    async fn pool_unlock_reports_a_real_failure_without_timing_out() {
        // Exercises `verdict`'s `--pool` wording; no wedge involved.
        let rrid = "SUSE:Maintenance:1:1";
        let conn = MockConnection::new("broken")
            .with_file(POOL_LOCK_PATH, own_pool_claim(rrid))
            .failing_sftp_remove();
        let mut broken = target("broken", conn.clone());
        broken.set_rrid(rrid);
        let (mut session, buf) = session_with_targets(rrid, vec![broken]);

        let args = matches(&HostsUnlock, &["-p"]);
        let err = HostsUnlock.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("pool unlock failed on: broken")),
            "{err}"
        );
        assert!(
            buf.contents().contains("broken: FAILED"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn pool_unlock_is_bounded_when_a_host_wedges() {
        // Mutation to catch: dropping `bounded_unlock`'s `timeout` wrapper must
        // hang this past its own 5s guard, not pass silently.
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![target(
                "dead",
                MockConnection::new("dead").with_sftp_session_delay(Duration::from_secs(3600)),
            )],
        );

        let args = matches(&HostsUnlock, &["-p"]);
        let err = testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                Duration::from_secs(5),
                HostsUnlock.call(&mut session, &args),
            )
            .await
            .expect("pool unlock must return despite the wedged host")
            .expect_err("a host that never answered must not report success")
        })
        .await;

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("dead")),
            "{err}"
        );
        let out = buf.contents();
        assert!(!out.contains("pool claim removed on"), "{out}");
        assert!(
            out.contains("dead: pool claim release not confirmed"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn pool_unlock_timeout_names_only_the_wedged_host() {
        // `healthy` needs both the seeded `POOL_LOCK_PATH` and a matching RRID,
        // or `is_locked()` short-circuits to `Ok(false)` before any wedge and the
        // "really removed" half asserts nothing.
        let rrid = "SUSE:Maintenance:1:1";
        let healthy_conn =
            MockConnection::new("healthy").with_file(POOL_LOCK_PATH, own_pool_claim(rrid));
        let mut healthy = target("healthy", healthy_conn.clone());
        healthy.set_rrid(rrid);
        let dead = target(
            "dead",
            MockConnection::new("dead").with_sftp_session_delay(Duration::from_secs(3600)),
        );
        let (mut session, buf) = session_with_targets(rrid, vec![dead, healthy]);

        let args = matches(&HostsUnlock, &["-p"]);
        let err = testkit::with_shrunk_budget(50, async {
            tokio::time::timeout(
                Duration::from_secs(5),
                HostsUnlock.call(&mut session, &args),
            )
            .await
            .expect("pool unlock must return despite the wedged host")
            .expect_err("a wedged host must not report success")
        })
        .await;

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("dead")),
            "{err}"
        );
        assert!(
            !matches!(&err, CommandError::Other(m) if m.contains("healthy")),
            "the host whose claim really released must not be named as timed out: {err}"
        );
        let out = buf.contents();
        assert!(
            out.contains("dead: pool claim release not confirmed"),
            "{out}"
        );
        assert!(out.contains("healthy: pool claim removed"), "{out}");
        assert!(
            healthy_conn.file_contents(POOL_LOCK_PATH).is_none(),
            "the reachable host's pool claim was still removed"
        );
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
