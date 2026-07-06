//! The `add_host` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::Target;
use mtui_types::enums::{ExecutionMode, TargetState};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Adds one or more reference hosts to the target host list.
///
/// Ports the container half of upstream `mtui.commands.addhost.AddHost`. Each
/// `-t`/`--target` host is added to the active report's group.
///
/// **Scope note (deferred):** upstream also, when no `-t` is given, resolves the
/// report's testplatforms through the refhosts factory and *connects* the added
/// hosts. That refhosts-resolution + autoconnect path depends on machinery still
/// deferred in `mtui-datasources`/`mtui-testreport` (tracked as a follow-up), so
/// this port requires explicit `-t` hosts and adds them **unconnected** — the
/// live SSH connect is the deferred half. The upstream auto→manual workflow
/// switch is likewise deferred with its testplatform path.
pub struct AddHost;

#[async_trait]
impl Command for AddHost {
    fn name(&self) -> &'static str {
        "add_host"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("target")
                .short('t')
                .long("target")
                .value_name("HOST")
                .action(ArgAction::Append)
                .help("Address of the target host (FQDN). Can be repeated"),
        )
        .arg(
            Arg::new("keep_mode")
                .short('k')
                .long("keep-mode")
                .action(ArgAction::SetTrue)
                .help("Do not switch to the manual workflow when in automatic mode"),
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let hosts: Vec<String> = args
            .get_many::<String>("target")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();

        if hosts.is_empty() {
            return Err(CommandError::Other(
                "add_host without -t (testplatform autoconnect) is not yet available; \
                 name hosts explicitly with -t"
                    .to_owned(),
            ));
        }

        let config = session.config.clone();
        let targets = session.targets_mut();
        for host in hosts {
            // Added unconnected: the live SSH connect is the deferred half.
            let target = Target::new(&config, host, TargetState::Enabled, ExecutionMode::Parallel);
            targets.add(target);
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
        assert_eq!(AddHost.name(), "add_host");
        assert_eq!(AddHost.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn adds_named_hosts_to_the_group() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&AddHost, &["-t", "h2", "-t", "h3"]);
        AddHost.call(&mut session, &args).await.unwrap();
        let names = session.targets().names();
        assert!(names.contains(&"h2".to_owned()));
        assert!(names.contains(&"h3".to_owned()));
        assert_eq!(session.targets().len(), 3);
    }

    #[tokio::test]
    async fn without_target_is_deferred_error() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&AddHost, &[]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("not yet available")));
    }
}
