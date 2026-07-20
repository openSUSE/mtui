//! The `reboot` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, complete_fanout, named_hosts};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Reboots reference hosts and reconnects once they are back up.
///
/// Ports upstream `mtui.commands.reboot.Reboot`. Reboots every connected host
/// (or only those given with `-t`), dispatching the reboot without waiting (the
/// SSH connection is expected to drop), then reconnecting each with retries and
/// backoff. Works for transactional and non-transactional hosts.
///
/// While testing a Product Increment, the per-host testing lock is re-applied
/// after the reboot (a reboot clears `/var/lock`), so it is not lost — the
/// report's `lock_comment` carries the relock comment (empty when no PI
/// assignment is active).
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
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // Honour `-t`: reboot only the selected hosts of the active template.
        // Reject an explicit host that is not connected (upstream `parse_hosts`);
        // the deprecated `all` sentinel means every connected host. Without `-t`
        // (or with `all`) every connected host of the fan-out–selected template
        // is rebooted. An empty group is `NoRefhostsDefined`.
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
        // Upstream `targets.reboot` uses the default reboot command; the group's
        // reboot drops each selected connection, reconnects, and re-applies the
        // lock (to the selected hosts) when `relock` is non-empty.
        let outcomes = targets.reboot_selected("reboot", &relock, &selected).await;

        // Report each host: `Ok` means it rebooted (boot id changed) and
        // reconnected; `Err` means the reconnect failed or the boot id was
        // unchanged (the host never rebooted). Fail if any host failed so an MCP
        // caller never sees a silent "success" on a host that did not reboot.
        let mut failed: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                Ok(()) => session
                    .display
                    .println(&format!("{host}: rebooted & reconnected")),
                Err(reason) => {
                    session
                        .display
                        .println(&format!("{host}: FAILED ({reason})"));
                    failed.push(host.clone());
                }
            }
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
        assert_eq!(Reboot.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn reboots_connected_hosts() {
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", true)]);
        let args = matches(&Reboot, &[]);
        Reboot.call(&mut session, &args).await.unwrap();
        // The group is preserved (reboot mutates in place, does not drop hosts).
        assert_eq!(session.targets().names(), vec!["h1", "h2"]);
        let out = buf.contents();
        assert!(out.contains("h1: rebooted & reconnected"), "{out}");
        assert!(out.contains("h2: rebooted & reconnected"), "{out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    #[tokio::test]
    async fn target_selection_reboots_only_named_host() {
        // Regression for mtui-rs-issz: `-t h1` must reboot only h1 and leave h2
        // untouched. Both hosts would reboot cleanly if the whole group were
        // rebooted, so the absence of any h2 line proves h2 was skipped.
        let (mut session, buf) =
            session_with_reboot_outcomes("SUSE:Maintenance:1:1", &[("h1", true), ("h2", true)]);
        let args = matches(&Reboot, &["-t", "h1"]);
        Reboot.call(&mut session, &args).await.unwrap();
        // The group is preserved intact (both hosts remain members).
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
    async fn unknown_named_host_is_not_connected() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Reboot, &["-t", "ghost"]);
        let err = Reboot.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::HostNotConnected(h) if h == "ghost"));
    }
}
