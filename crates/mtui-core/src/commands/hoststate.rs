//! The `set_host_state` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};
use mtui_types::enums::{ExecutionMode, TargetState};

use super::support::{add_hosts_arg, select_names};
use crate::command::Command;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The five `state` choices upstream accepts.
const STATES: [&str; 5] = ["parallel", "serial", "dryrun", "disabled", "enabled"];

/// Sets the state and execution mode of a host.
///
/// Ports upstream `mtui.commands.hoststate.HostState`. A host can be:
/// * `enabled` — runs all issued commands,
/// * `disabled` — runs nothing,
/// * `dryrun` — runs nothing but prints the commands,
///
/// and its execution mode either `parallel` (default) or `serial`. The
/// `serial`/`parallel` choices set the mode; the others set the state. Selection
/// acts on named hosts (or all, disabled included) exactly like upstream's
/// `parse_hosts(enabled=False)`.
pub struct HostState;

#[async_trait]
impl Command for HostState {
    fn name(&self) -> &'static str {
        "set_host_state"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Sets the state and execution mode of a host.")
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("state")
                .required(true)
                .value_parser(clap::builder::PossibleValuesParser::new(STATES))
                .help("enabled | disabled | dryrun | parallel | serial"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        // Complete both the state choices and the loaded host names.
        STATES
            .into_iter()
            .map(str::to_owned)
            .chain(session.targets().names())
            .filter(|c| c.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let state = args
            .get_one::<String>("state")
            .expect("state is required")
            .clone();

        let targets = session.targets_mut();
        // enabled=false: state changes apply to disabled hosts too.
        let hosts =
            select_names(targets, args, false).map_err(|e| CommandError::Other(e.to_string()))?;

        for name in &hosts {
            let Some(t) = targets.get_mut(name) else {
                continue;
            };
            match state.as_str() {
                "serial" => t.set_mode(ExecutionMode::Serial),
                "parallel" => t.set_mode(ExecutionMode::Parallel),
                "enabled" => t.set_state(TargetState::Enabled),
                "disabled" => t.set_state(TargetState::Disabled),
                "dryrun" => t.set_state(TargetState::Dryrun),
                other => return Err(CommandError::Other(format!("unknown state: {other}"))),
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
    fn name_is_set_host_state() {
        assert_eq!(HostState.name(), "set_host_state");
    }

    #[tokio::test]
    async fn disabled_sets_state() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostState, &["disabled", "-t", "h1"]);
        HostState.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().state(),
            TargetState::Disabled
        );
    }

    #[tokio::test]
    async fn dryrun_sets_state() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostState, &["dryrun"]);
        HostState.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().state(),
            TargetState::Dryrun
        );
    }

    #[tokio::test]
    async fn serial_sets_mode_not_state() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostState, &["serial"]);
        HostState.call(&mut session, &args).await.unwrap();
        let t = session.targets().get("h1").unwrap();
        assert_eq!(t.mode(), ExecutionMode::Serial);
        // State untouched.
        assert_eq!(t.state(), TargetState::Enabled);
    }

    #[tokio::test]
    async fn applies_even_to_disabled_hosts() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // Disable, then re-enable via the command — proves enabled=false.
        session
            .targets_mut()
            .get_mut("h1")
            .unwrap()
            .set_state(TargetState::Disabled);
        let args = matches(&HostState, &["enabled", "-t", "h1"]);
        HostState.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().state(),
            TargetState::Enabled
        );
    }

    #[tokio::test]
    async fn unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&HostState, &["enabled", "-t", "ghost"]);
        let err = HostState.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[test]
    fn invalid_state_rejected_by_parser() {
        let base = clap::Command::new(HostState.name()).no_binary_name(true);
        let parsed = HostState.configure(base).try_get_matches_from(["bogus"]);
        assert!(parsed.is_err());
    }
}
