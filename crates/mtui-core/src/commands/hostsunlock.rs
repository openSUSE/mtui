//! The `unlock` command (host operation lock / pool claim).

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Unlocks hosts previously locked with `lock`.
///
/// Ports upstream `mtui.commands.hostsunlock.HostsUnlock`. By default removes the
/// zypper/operation lock; `-f`/`--force` also removes locks set by other users
/// or sessions.
///
/// `-p`/`--pool` removes the host *pool* claim (RRID-based ownership) instead of
/// the zypper/operation lock, fanning [`HostsGroup::pool_unlock`] out across the
/// active group. With `--force` a claim owned by another template is removed too.
///
/// Like `lock`, host sub-selection via `-t` is not yet honoured for the fan-out
/// (whole active group), matching the group-merge follow-up (`mtui-rs-qd9`).
pub struct HostsUnlock;

#[async_trait]
impl Command for HostsUnlock {
    fn name(&self) -> &'static str {
        "unlock"
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
            session.targets_mut().pool_unlock(force).await;
            return Ok(());
        }
        // The group `unlock` fan-out always passes force=false; iterate targets
        // so `--force` can remove foreign locks. Best-effort, like the group.
        let targets = session.targets_mut();
        for name in targets.names() {
            if let Some(t) = targets.get_mut(&name) {
                t.unlock(force).await;
            }
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
        assert_eq!(HostsUnlock.name(), "unlock");
        assert_eq!(HostsUnlock.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn unlock_op_lock_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &[]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn unlock_force_succeeds() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-f"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn pool_unlock_routes_to_pool_branch() {
        // `--pool` fans HostsGroup::pool_unlock out over the group. On an
        // unclaimed host this is a clean no-op (upstream routes to
        // `hosts.pool_unlock(force=False)`); the command must succeed rather than
        // return the old deferred error.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-p"]);
        HostsUnlock.call(&mut session, &args).await.unwrap();
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
