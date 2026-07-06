//! The `remove_host` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Disconnects from a host and removes it from the list.
///
/// Ports the container half of upstream `mtui.commands.removehost.RemoveHost`.
/// Dropping the removed [`Target`](mtui_hosts::Target) closes its owned
/// connection (upstream disconnects and purges the host log). With no `-t`
/// argument every host is removed.
pub struct RemoveHost;

#[async_trait]
impl Command for RemoveHost {
    fn name(&self) -> &'static str {
        "remove_host"
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
        let targets = session.targets_mut();
        // enabled=false: remove disabled hosts too (upstream parse_hosts(enabled=False)).
        let hosts =
            select_names(targets, args, false).map_err(|e| CommandError::Other(e.to_string()))?;
        for name in &hosts {
            targets.remove(name);
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

    #[test]
    fn complete_offers_host_names() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert_eq!(
            RemoveHost.complete(&session, "h", "remove_host h"),
            vec!["h1".to_owned()]
        );
    }
}
