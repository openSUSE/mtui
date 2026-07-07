//! The `list_hosts` command.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::system::System;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Lists all connected hosts with their system, state, and execution mode.
///
/// Ports upstream `mtui.commands.simplelists.ListHosts`, which calls
/// `targets.report_self(display.list_host)`. Each host's status tuple
/// (`hostname, system, transactional, state, mode` — the
/// [`Reporter::self_`](mtui_hosts) fields) is snapshotted first so the report
/// borrow does not overlap the display's mutable borrow, then rendered through
/// the display's `list_host` sink.
pub struct ListHosts;

/// One host's full status tuple, snapshotted for rendering.
type HostStatus = (String, System, bool, TargetState, ExecutionMode);

#[async_trait]
impl Command for ListHosts {
    fn name(&self) -> &'static str {
        "list_hosts"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rows: Vec<HostStatus> = session
            .targets()
            .targets()
            .map(|t| {
                (
                    t.hostname().to_owned(),
                    t.system().clone(),
                    t.transactional(),
                    t.state(),
                    t.mode(),
                )
            })
            .collect();
        for (name, system, transactional, state, mode) in rows {
            session
                .display
                .list_host(&name, &system, transactional, state, mode);
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
        assert_eq!(ListHosts.name(), "list_hosts");
        assert_eq!(ListHosts.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn lists_connected_hosts() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&ListHosts, &[]);
        ListHosts.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1"), "{out}");
        assert!(out.contains("h2"), "{out}");
        assert!(out.contains("Enabled"), "{out}");
    }
}
