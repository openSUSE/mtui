//! The `set_timeout` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Sets the command-execution timeout, in seconds, for the selected hosts.
///
/// Ports upstream `mtui.commands.simpleset.SetTimeout`, which calls
/// `target.set_timeout(value)` per host. `0` disables the timeout. Selection
/// acts on the named `-t` hosts, or all enabled hosts when omitted.
///
/// A host-phase command that takes only `-t/--target` (upstream `_add_hosts_arg`
/// without `_add_template_arg`), so it is [`Scope::Active`] to match upstream:
/// it acts on the active template's host set, not once per loaded template.
pub struct SetTimeout;

#[async_trait]
impl Command for SetTimeout {
    fn name(&self) -> &'static str {
        "set_timeout"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Sets the command-execution timeout, in seconds, for the selected hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Active
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("timeout")
                .required(true)
                .value_parser(clap::value_parser!(u64))
                .help("Timeout in seconds; \"0\" disables it"),
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
        let value = *args.get_one::<u64>("timeout").expect("timeout is required");

        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        for name in &hosts {
            if let Some(t) = targets.get_mut(name) {
                t.set_timeout(value);
            }
        }
        session
            .display
            .println(&format!("timeout set to {value}s on {}", hosts.join(", ")));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};

    #[test]
    fn name_and_active_scope() {
        assert_eq!(SetTimeout.name(), "set_timeout");
        // Upstream `SetTimeout` is `-t`-only (no `_add_template_arg`), so it
        // stays active rather than fanning out per loaded template.
        assert_eq!(SetTimeout.scope(), Scope::Active);
    }

    #[tokio::test]
    async fn sets_timeout_on_host() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SetTimeout, &["300", "-t", "h1"]);
        SetTimeout.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().get("h1").unwrap().timeout_secs(), 300);
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("timeout set to 300s on h1"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn zero_disables_timeout() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SetTimeout, &["0", "-t", "h1"]);
        SetTimeout.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().get("h1").unwrap().timeout_secs(), 0);
    }

    #[tokio::test]
    async fn rejects_non_numeric_timeout() {
        // clap rejects a non-u64 value at parse time.
        let base = clap::Command::new("set_timeout").no_binary_name(true);
        let parsed = SetTimeout
            .configure(base)
            .try_get_matches_from(["notanumber"]);
        assert!(parsed.is_err());
    }
}
