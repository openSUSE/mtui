//! The `set_timeout` command.

use std::num::NonZeroU64;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Sets the command-execution timeout, in seconds, for the selected hosts.
///
/// The value must be positive and reaches already-connected hosts
/// immediately. Acts on the named `-t` hosts, or all enabled hosts.
///
/// `-t/--target`-only, hence [`Scope::Active`]: it acts on the active template's
/// host set, not once per loaded template.
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
                .value_parser(clap::value_parser!(u64).range(1..))
                .help("Timeout in seconds; must be positive"),
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
        // `range(1..)` rejects 0 at parse time, so this only fires for an
        // `ArgMatches` built without the parser.
        let Some(secs) = NonZeroU64::new(value) else {
            return Err(CommandError::Other(format!(
                "timeout must be positive, got {value}"
            )));
        };

        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        for name in &hosts {
            if let Some(t) = targets.get_mut(name) {
                t.set_timeout(secs);
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
        // `-t`-only, so it stays active rather than fanning out per template.
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

    #[test]
    fn zero_timeout_is_rejected_at_parse() {
        // 0 does not disable the timeout: a zero window would give a
        // headless run a deadline of now, so `range(1..)` rejects it with a
        // usage error like `-w/--connection-timeout` does.
        let base = clap::Command::new("set_timeout").no_binary_name(true);
        let parsed = SetTimeout
            .configure(base)
            .try_get_matches_from(["0", "-t", "h1"]);
        assert!(parsed.is_err());
    }

    #[tokio::test]
    async fn rejects_non_numeric_timeout() {
        let base = clap::Command::new("set_timeout").no_binary_name(true);
        let parsed = SetTimeout
            .configure(base)
            .try_get_matches_from(["notanumber"]);
        assert!(parsed.is_err());
    }
}
