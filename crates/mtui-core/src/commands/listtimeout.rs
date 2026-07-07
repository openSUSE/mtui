//! The `list_timeout` command.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_types::system::System;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Prints the current command timeout per host, in seconds.
///
/// Ports upstream `mtui.commands.simplelists.ListTimeout`, which calls
/// `targets.report_timeout(display.list_timeout)`. Each host's
/// `(hostname, system, timeout_secs)` is snapshotted first (the
/// [`Reporter::timeout`](mtui_hosts) fields), then rendered through the
/// display's `list_timeout` sink.
pub struct ListTimeout;

#[async_trait]
impl Command for ListTimeout {
    fn name(&self) -> &'static str {
        "list_timeout"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rows: Vec<(String, System, u64)> = session
            .targets()
            .targets()
            .map(|t| {
                (
                    t.hostname().to_owned(),
                    t.system().clone(),
                    t.timeout_secs(),
                )
            })
            .collect();
        for (name, system, secs) in rows {
            session.display.list_timeout(&name, &system, secs);
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
        assert_eq!(ListTimeout.name(), "list_timeout");
        assert_eq!(ListTimeout.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn prints_seconds_per_host() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&ListTimeout, &[]);
        ListTimeout.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("h1"), "{out}");
        assert!(out.contains('s'), "{out}");
    }
}
