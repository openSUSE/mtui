//! The `add_host` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_types::Workflow;
use tracing::info;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Adds one or more reference hosts to the target host list.
///
/// Ports upstream `mtui.commands.addhost.AddHost`. Running `add_host` is a manual
/// action, so if the session is still in the automatic workflow it is moved to
/// manual (unless `-k`/`--keep-mode`). Then:
///
/// * **with `-t`/`--target`:** each named host is connected and added to the
///   active report's group ([`Session::add_named_hosts`]).
/// * **without `-t`:** the report's testplatforms are resolved through the
///   refhosts factory and the resulting hosts are connected and added
///   ([`Session::add_testplatform_hosts`]).
///
/// **Deviation:** upstream additionally calls `prompt.set_prompt()` after the
/// workflow switch to refresh the REPL prompt string; that is a Phase-6 REPL
/// concern, so this command only mutates the report's workflow.
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
        // Running add_host is a manual action. If the session is still in the
        // automatic workflow the user almost certainly meant to test manually
        // (and just forgot to switch), so move to manual — unless --keep-mode.
        let keep_mode = args.get_flag("keep_mode");
        if session.metadata().workflow() == Workflow::Auto && !keep_mode {
            info!("add_host: switching from automatic to manual workflow");
            session.set_workflow(Workflow::Manual);
        }

        let hosts: Vec<String> = args
            .get_many::<String>("target")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();

        if hosts.is_empty() {
            // No -t: resolve the report's testplatforms and connect the hosts.
            session.add_testplatform_hosts().await;
        } else {
            // Explicit -t: connect and add each named host.
            session.add_named_hosts(hosts).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use crate::commands::testkit::{matches, session_with_hosts};

    /// Path to the ported offline refhosts fixture (no network).
    const REFHOSTS_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mtui-datasources/tests/fixtures/refhosts.yml"
    );

    /// Points the session's config at the offline `path` refhosts resolver.
    fn use_path_refhosts(session: &mut Session) {
        session.config.refhosts_resolvers = "path".to_owned();
        session.config.refhosts_path = REFHOSTS_FIXTURE.into();
    }

    /// Sets the active report's testplatforms (via the public template registry).
    fn set_testplatforms(session: &mut Session, tps: &[&str]) {
        session.templates.active_mut().base_mut().testplatforms =
            tps.iter().map(|s| (*s).to_owned()).collect();
    }

    /// Pre-adds an already-connected (mock) target named `host` to the active
    /// group, so a subsequent `add_host -t host` sees `connect` short-circuit.
    fn add_mock_host(session: &mut Session, host: &str) {
        let conn = MockConnection::new(host).with_default(CommandLog::new("", "ok", "", 0, 0));
        let target = Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Serial,
            Box::new(conn),
        );
        session.targets_mut().add(target);
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(AddHost.name(), "add_host");
        assert_eq!(AddHost.scope(), Scope::Fanout);
    }

    /// Explicit `-t` hosts are connected and added to the active group. The
    /// hosts named here are not backed by a mock connection, so their live
    /// connect fails and they are skipped — the group keeps only the host it
    /// started with. (The connect-and-add path itself is exercised over a mock
    /// connection in `connects_and_adds_a_mock_backed_host`.)
    #[tokio::test]
    async fn named_hosts_that_cannot_connect_are_skipped() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        AddHost.call(&mut session, &args).await.unwrap();
        // The unreachable host could not connect, so it is not added.
        assert_eq!(session.targets().len(), 1);
        assert!(
            !session
                .targets()
                .names()
                .contains(&"unreachable.invalid".to_owned())
        );
    }

    /// A pre-connected mock host already in the group survives an `add_host` of
    /// a *different* unreachable host: the failed connect is skipped and the
    /// existing member is untouched (one bad host never disturbs the group).
    #[tokio::test]
    async fn existing_mock_host_survives_a_failed_add() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        add_mock_host(&mut session, "h2");
        let before = session.targets().len();
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        AddHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().len(), before);
        assert!(session.targets().names().contains(&"h2".to_owned()));
    }

    /// Running `add_host` in the automatic workflow switches to manual.
    #[tokio::test]
    async fn switches_auto_workflow_to_manual() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        AddHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// `--keep-mode` preserves the automatic workflow.
    #[tokio::test]
    async fn keep_mode_preserves_auto_workflow() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable.invalid", "-k"]);
        AddHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Auto);
    }

    /// A manual workflow is left untouched (no spurious downgrade path).
    #[tokio::test]
    async fn manual_workflow_is_left_untouched() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Manual);
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        AddHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// Without `-t`, add_host resolves the report's testplatforms via the
    /// offline `path` refhosts resolver and connects the resulting hosts. The
    /// resolved fixture hosts are not mock-backed, so they fail to connect and
    /// are skipped — but the testplatform-resolution path is driven end to end.
    #[tokio::test]
    async fn no_target_resolves_testplatforms() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        set_testplatforms(&mut session, &["base=sles(major=15,minor=5);arch=[x86_64]"]);
        let args = matches(&AddHost, &[]);
        AddHost.call(&mut session, &args).await.unwrap();
        // Resolution ran (no panic); unreachable fixture hosts were skipped, so
        // the group is unchanged.
        assert_eq!(session.targets().len(), 1);
    }
}
