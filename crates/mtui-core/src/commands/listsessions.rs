//! The `list_sessions` command.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_types::system::System;

use super::support::{add_hosts_arg, per_host, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The upstream `ss`-based session probe (verbatim, so remote output matches).
const SESSION_CMD: &str = r"ss -r  | sed -n 's/^[^:]*:ssh *\([^ ]*\):.*/\1/p' | sort -u";

/// Lists the active SSH sessions connected to each reference host.
///
/// Ports upstream `mtui.commands.simplelists.ListSessions`, which runs the
/// `ss`/`sed` probe on the selected hosts and then reports each host's last
/// stdout through `display.list_sessions`. The probe command is reproduced
/// verbatim so remote output matches upstream.
pub struct ListSessions;

#[async_trait]
impl Command for ListSessions {
    fn name(&self) -> &'static str {
        "list_sessions"
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
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        targets.run(per_host(SESSION_CMD, &hosts)).await;

        let rows: Vec<(String, System, String)> = hosts
            .iter()
            .filter_map(|name| {
                targets
                    .get(name)
                    .map(|t| (name.clone(), t.system().clone(), t.lastout().to_owned()))
            })
            .collect();
        for (name, system, stdout) in rows {
            session.display.list_sessions(&name, &system, &stdout);
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
        assert_eq!(ListSessions.name(), "list_sessions");
        assert_eq!(ListSessions.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn reports_probe_output() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "10.0.0.1\n");
        let args = matches(&ListSessions, &["-t", "h1"]);
        ListSessions.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("sessions on h1"), "{out}");
        assert!(out.contains("10.0.0.1"), "{out}");
    }

    #[tokio::test]
    async fn no_hosts_errors() {
        let (mut session, _buf) = crate::commands::testkit::empty_session();
        let args = matches(&ListSessions, &[]);
        let err = ListSessions.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[test]
    fn complete_offers_matching_host_names() {
        let (session, _buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["host-a", "host-b"], "ok");
        let got = ListSessions.complete(&session, "host-a", "list_sessions host-a");
        assert_eq!(got, vec!["host-a".to_owned()]);
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListSessions, &["-t", "ghost"]);
        let err = ListSessions.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
