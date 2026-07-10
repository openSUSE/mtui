//! The `list_locks` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_types::system::System;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::display::LockStatus;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Lists the lock state of all connected hosts.
///
/// Ports upstream `mtui.commands.simplelists.ListLocks`. By default only the
/// zypper/operation locks (set by `lock` and the install/update/prepare/downgrade
/// flows) are shown; `-p`/`--pool` instead lists the host *pool* claims taken
/// during pool selection.
///
/// Upstream does `self.targets.select(enabled=True).report_locks(...)`: the
/// enabled hosts (honouring the `-t` sub-selection) are resolved via
/// [`Target::lock_status`](mtui_hosts::Target::lock_status) and forwarded through
/// the per-host [`Reporter::locks`](mtui_hosts::Reporter) sink into
/// `display.list_locks`. The lock accessors are async `&mut self`; the resolved
/// (sync) [`LockStatus`] values are collected first so `display` — which borrows
/// the session mutably — is driven afterwards.
pub struct ListLocks;

#[async_trait]
impl Command for ListLocks {
    fn name(&self) -> &'static str {
        "list_locks"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the lock state of all connected hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("pool")
                .short('p')
                .long("pool")
                .action(ArgAction::SetTrue)
                .help("list pool-claim locks instead of zypper/operation locks"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        ["-p", "--pool"]
            .into_iter()
            .map(str::to_owned)
            .chain(session.targets().names())
            .filter(|s| s.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let pool = args.get_flag("pool");
        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }

        // Resolve each host's lock (async, &mut) into a sync row, then render.
        let mut rows: Vec<(String, System, LockStatus)> = Vec::with_capacity(hosts.len());
        for name in &hosts {
            let Some(target) = targets.get_mut(name) else {
                continue;
            };
            let row = target.lock_status(pool).await;
            let status = LockStatus {
                is_locked: row.is_locked,
                is_mine: row.is_mine,
                locked_by: row.locked_by,
                time: row.time,
                comment: row.comment,
            };
            rows.push((name.clone(), target.system().clone(), status));
        }

        for (name, system, status) in rows {
            session.display.list_locks(&name, &system, &status);
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
        assert_eq!(ListLocks.name(), "list_locks");
        assert_eq!(ListLocks.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn reports_not_locked_for_free_host() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListLocks, &["-t", "h1"]);
        ListLocks.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1"), "{out}");
        assert!(out.contains("not locked"), "{out}");
    }

    #[tokio::test]
    async fn no_hosts_errors() {
        let (mut session, _buf) = crate::commands::testkit::empty_session();
        let args = matches(&ListLocks, &[]);
        let err = ListLocks.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListLocks, &["-t", "ghost"]);
        let err = ListLocks.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn pool_flag_is_accepted() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListLocks, &["-p"]);
        ListLocks.call(&mut session, &args).await.unwrap();
        // A free host reports "not locked" on the pool path too.
        assert!(buf.contents().contains("not locked"));
    }

    #[test]
    fn complete_offers_pool_flag_and_matching_hosts() {
        let (session, _buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["host-a", "host-b"], "ok");
        let got = ListLocks.complete(&session, "-p", "list_locks -p");
        assert_eq!(got, vec!["-p".to_owned()]);
        let got = ListLocks.complete(&session, "host-a", "list_locks host-a");
        assert_eq!(got, vec!["host-a".to_owned()]);
    }
}
