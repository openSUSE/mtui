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
        // `HostsGroup::reboot` reboots the whole group; honour `-t` by rejecting
        // an explicit host that is not connected (upstream `parse_hosts`), then
        // reboot. An empty group is `NoRefhostsDefined`.
        if named_hosts(args) {
            let targets = session.targets();
            if let Some(hosts) = super::support::hosts_arg(args) {
                for name in &hosts {
                    if name != "all" && !targets.contains(name) {
                        return Err(CommandError::HostNotConnected(name.clone()));
                    }
                }
            }
        }

        let relock = session.metadata().base().lock_comment.clone();
        let targets = session.targets_mut();
        if targets.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        // Upstream `targets.reboot` uses the default reboot command; the group's
        // reboot drops each connection, reconnects, and re-applies the lock when
        // `relock` is non-empty.
        targets.reboot("reboot", &relock).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

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
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&Reboot, &[]);
        Reboot.call(&mut session, &args).await.unwrap();
        // The group is preserved (reboot mutates in place, does not drop hosts).
        assert_eq!(session.targets().names(), vec!["h1", "h2"]);
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
