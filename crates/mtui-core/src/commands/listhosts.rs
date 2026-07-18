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

    fn about(&self) -> Option<&'static str> {
        Some("Lists all connected hosts with their system, state, and execution mode.")
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
        if rows.is_empty() {
            session.display.println("No hosts connected.");
            return Ok(());
        }
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
    use mtui_hosts::{MockConnection, Target};

    use super::*;
    use crate::commands::testkit::{
        empty_session, matches, session_with_hosts, session_with_targets,
    };

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListHosts.name(), "list_hosts");
        assert_eq!(ListHosts.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn no_hosts_prints_none_line() {
        let (mut session, buf) = empty_session();
        let args = matches(&ListHosts, &[]);
        ListHosts.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("No hosts connected."),
            "{}",
            buf.contents()
        );
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

    /// Regression test for mtui-rs-xlt: a host whose `Target::connect()` ran
    /// over a connection carrying real system-parse data must render its
    /// parsed system, not the pre-parse `unknown--` sentinel.
    #[tokio::test]
    async fn connected_host_shows_parsed_system_not_unknown_sentinel() {
        let prod = br#"<product><name>SLES</name><baseversion>15</baseversion><patchlevel>5</patchlevel><arch>x86_64</arch></product>"#;
        let conn = MockConnection::new("dove.qam.suse.cz")
            .with_listing("/etc/products.d", ["SLES.prod"])
            .with_link("/etc/products.d/baseproduct", "SLES.prod")
            .with_file("/etc/products.d/SLES.prod", prod.to_vec());
        let mut target = Target::with_connection(
            "dove.qam.suse.cz",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
        target.connect().await.expect("connect parses the system");

        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![target]);
        let args = matches(&ListHosts, &[]);
        ListHosts.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("sles-15-SP5-x86_64"), "{out}");
        assert!(!out.contains("unknown--"), "{out}");
    }
}
