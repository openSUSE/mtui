//! The `reboot` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, complete_fanout, named_hosts};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Reboots reference hosts and reconnects once they are back up.
///
/// Reboots every connected host (or only those given with `-t`), dispatching
/// without waiting since the SSH connection is expected to drop, then
/// reconnecting each with retries and backoff. Transactional or not.
///
/// A reboot clears `/var/lock` (tmpfs), so the reservations (`lock -c`) and
/// pool claims this session held are re-applied once the host is back:
/// another owner's lock is never overwritten. A hold that could not be
/// re-taken is reported on its own display line naming the host and the
/// hold — the reboot outcome itself stays a success — alongside the usual
/// per-host verdicts. A Product Increment's per-host testing lock (from the report's
/// `lock_comment`, empty when no PI assignment is active) is re-applied the
/// same way.
pub struct Reboot;

#[async_trait]
impl Command for Reboot {
    fn name(&self) -> &'static str {
        "reboot"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Reboots reference hosts and reconnects once they are back up.")
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // An explicit host that is not connected is rejected; the deprecated
        // `all` sentinel, like no `-t` at all, means every connected host.
        let targets = session.targets();
        if targets.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        let all_names: std::collections::BTreeSet<String> = targets.names().into_iter().collect();
        let selected: std::collections::BTreeSet<String> = if named_hosts(args) {
            match super::support::hosts_arg(args) {
                Some(hosts) if hosts.iter().any(|h| h == "all") => all_names.clone(),
                Some(hosts) => {
                    for name in &hosts {
                        if !all_names.contains(name) {
                            return Err(CommandError::HostNotConnected(name.clone()));
                        }
                    }
                    hosts.into_iter().collect()
                }
                None => all_names.clone(),
            }
        } else {
            all_names.clone()
        };

        let relock = session.metadata().base().lock_comment.clone();
        let targets = session.targets_mut();
        let report = targets.reboot_selected("reboot", &relock, &selected).await;
        let outcomes = &report.outcomes;

        // `Err` is a failed reconnect *or* an unchanged boot id (the host never
        // rebooted); either must fail the command, so an MCP caller never sees a
        // silent success on a host that did not reboot.
        let mut failed: Vec<String> = Vec::new();
        let mut ok: Vec<String> = Vec::new();
        let mut failed_lines: Vec<String> = Vec::new();
        for (host, outcome) in outcomes {
            match outcome {
                Ok(()) => ok.push(host.clone()),
                Err(reason) => {
                    failed_lines.push(format!("{host}: FAILED ({reason})"));
                    failed.push(host.clone());
                }
            }
        }
        // Identical successes share one verdict; failures stay per-host.
        // `outcomes` is a BTreeMap so `ok` is already sorted.
        if ok.len() > 1 {
            session
                .display
                .println(&format!("rebooted & reconnected on {}", ok.join(", ")));
        } else if let Some(host) = ok.first() {
            session
                .display
                .println(&format!("{host}: rebooted & reconnected"));
        }
        for line in &failed_lines {
            session.display.println(line);
        }
        // A lost hold is degradation, not failure: the host did reboot, but
        // the reservation or pool claim it carried is gone. Each gets its own
        // display line so an unattended MCP caller sees it.
        for loss in &report.lost_holds {
            session.display.println(&loss.describe());
        }

        if failed.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "reboot failed on: {}",
                failed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{
        empty_session, matches, session_with_hosts, session_with_reboot_outcomes,
    };

    #[tokio::test]
    async fn lost_hold_is_printed_on_the_display() {
        // A reservation lost in the reboot gap must reach the display (and so
        // an MCP caller), not only the server log — while the reboot outcome
        // itself stays a success.
        use mtui_hosts::{Connection, MockConnection, TARGET_LOCK_PATH, Target};
        use mtui_types::enums::TargetState;
        use mtui_types::hostlog::CommandLog;

        use crate::commands::testkit::session_with_targets;

        let mock = MockConnection::new("h1")
            .with_default(CommandLog::new("", "", "", 0, 0))
            .with_changing_boot_id();
        let mut handle = mock.clone();
        let mut target = Target::with_connection("h1", TargetState::Enabled, Box::new(mock));
        target.lock("operator hold").await.expect("setup lock held");
        handle
            .sftp_remove(std::path::Path::new(TARGET_LOCK_PATH))
            .await
            .expect("wipe");
        handle
            .sftp_write(
                std::path::Path::new(TARGET_LOCK_PATH),
                b"1700000000:alice:4242:busy",
                false,
            )
            .await
            .expect("foreign claim");
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![target]);

        let args = matches(&Reboot, &[]);
        Reboot.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("rebooted & reconnected"), "{out}");
        assert!(
            out.contains("reboot cleared the operation lock")
                && out.contains("operator hold")
                && out.contains("alice"),
            "lost hold named on the display: {out}"
        );
    }

    #[test]
    fn complete_offers_target_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Reboot.complete(&session, "", "reboot ");
        assert!(
            out.contains(&"-t".to_owned()) && out.contains(&"h1".to_owned()),
            "{out:?}"
        );
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Reboot.name(), "reboot");
        assert_eq!(Reboot.scope(), Scope::Explicit);
    }

    #[tokio::test]
    async fn reboots_connected_hosts() {
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", true)]);
        let args = matches(&Reboot, &[]);
        Reboot.call(&mut session, &args).await.unwrap();
        // Reboot mutates in place and drops no host.
        assert_eq!(session.targets().names(), vec!["h1", "h2"]);
        let out = buf.contents();
        assert!(out.contains("rebooted & reconnected on h1, h2"), "{out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    #[tokio::test]
    async fn target_selection_reboots_only_named_host() {
        // Both hosts would reboot cleanly if the whole group were rebooted, so
        // the absence of any h2 line proves `-t h1` skipped it.
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", true)]);
        let args = matches(&Reboot, &["-t", "h1"]);
        Reboot.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().names(), vec!["h1", "h2"]);
        let out = buf.contents();
        assert!(out.contains("h1: rebooted & reconnected"), "{out}");
        assert!(
            !out.contains("h2"),
            "h2 was not selected and must be untouched: {out}"
        );
    }

    #[tokio::test]
    async fn one_host_never_rebooted_errors_and_reports_both() {
        // h2's boot id is unchanged → recorded failure; h1 rebooted cleanly.
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", false)]);
        let args = matches(&Reboot, &[]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        match err {
            CommandError::Other(msg) => {
                assert!(msg.contains("h2"), "{msg}");
                assert!(!msg.contains("h1"), "only h2 failed: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        let out = buf.contents();
        assert!(out.contains("h1: rebooted & reconnected"), "{out}");
        assert!(out.contains("h2: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Reboot, &[]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn two_successes_and_a_failure_combine_successes() {
        let (mut session, buf) = session_with_reboot_outcomes(
            "SUSE:Maintenance:1:1",
            &[("h1", true), ("h2", true), ("h3", false)],
        );
        let args = matches(&Reboot, &[]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("h3")));
        let out = buf.contents();
        assert!(out.contains("rebooted & reconnected on h1, h2"), "{out}");
        assert!(out.contains("h3: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn all_fail_prints_only_failures() {
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", false), ("h2", false)]);
        let args = matches(&Reboot, &[]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("h1") && m.contains("h2")));
        let out = buf.contents();
        assert!(!out.contains("rebooted & reconnected"), "{out}");
        assert!(out.contains("h1: FAILED"), "{out}");
        assert!(out.contains("h2: FAILED"), "{out}");
    }

    #[tokio::test]
    async fn unknown_named_host_is_not_connected() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Reboot, &["-t", "ghost"]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::HostNotConnected(h) if h == "ghost"));
    }
}
