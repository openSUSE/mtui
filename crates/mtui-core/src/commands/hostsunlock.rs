//! The `unlock` command (host operation lock / pool claim).

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Unlocks hosts previously locked with `lock`.
///
/// Ports upstream `mtui.commands.hostsunlock.HostsUnlock`. By default removes the
/// zypper/operation lock; `-f`/`--force` also removes locks set by other users
/// or sessions.
///
/// `-p`/`--pool` (remove the host *pool* claim instead) is **not yet wired**:
/// the pool-claim lock requires per-`Target` `PoolLock` wiring that is still
/// deferred in `mtui-hosts` (tracked as a follow-up). Passing `--pool` returns a
/// clear error rather than silently removing the wrong lock.
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
        if args.get_flag("pool") {
            return Err(CommandError::Other(
                "pool-claim unlock (--pool) is not yet available: per-host pool-lock wiring \
                 is deferred in mtui-hosts"
                    .to_owned(),
            ));
        }
        let force = args.get_flag("force");
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
    async fn pool_unlock_is_deferred_error() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostsUnlock, &["-p"]);
        let err = HostsUnlock.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("not yet available")));
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
