//! The `remove_host` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Disconnects from a host and removes it from the list.
///
/// Ports upstream `mtui.commands.removehost.RemoveHost._remove_target`: for each
/// selected host it [`close`](mtui_hosts::Target::close)s the target (dropping
/// the remote operation and pool-claim lock files), releases the in-process
/// arbiter claim via
/// [`TestReport::release_pool_claim`](mtui_testreport::TestReport::release_pool_claim)
/// (without which a scarce-pool host stays marked busy in the process-global
/// [`HostArbiter`](mtui_hosts::HostArbiter) for the rest of a long-lived MCP
/// session), removes it from the group, and drops its `systems` entry. With no
/// `-t` argument every host is removed.
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
        // enabled=false: remove disabled hosts too (upstream parse_hosts(enabled=False)).
        let hosts = select_names(session.targets_mut(), args, false)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        for name in &hosts {
            // Take the target out and close it so the remote operation + pool
            // lock files drop (upstream `target.close()`); dropping the target
            // alone never runs `close`.
            if let Some(mut target) = session.targets_mut().remove(name) {
                target.close(None).await;
            }
            // Release the in-process arbiter claim + prune slot candidates
            // (upstream `metadata.release_pool_claim`); no-op when unpooled.
            session.metadata_mut().release_pool_claim(name);
            // Drop the per-host system entry (upstream `del metadata.systems`).
            session
                .metadata_mut()
                .base_mut()
                .systems
                .remove(name.as_str());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(RemoveHost.name(), "remove_host");
        assert_eq!(RemoveHost.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn removes_named_host() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert!(!session.targets().contains("h1"));
        assert!(session.targets().contains("h2"));
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

        // A test-local arbiter leaked to the `&'static` the report field needs,
        // without touching the process-global singleton.
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        // Wire pool state onto the active report: claim h1 for this owner.
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

        // Host is gone from the group and its in-process claim is released, so a
        // sibling session can re-acquire the freed pool host.
        assert!(!session.targets().contains("h1"));
        assert!(arbiter.try_acquire("h1", &other));
        // The report's claim bookkeeping no longer tracks it.
        assert!(!session.metadata().base().pool_claims.contains("h1"));
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
