//! The `add_host` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_types::Workflow;
use tracing::info;

use super::support::complete_with_templates;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
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

    fn about(&self) -> Option<&'static str> {
        Some("Adds one or more reference hosts to the target host list.")
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

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(
            session,
            &[&["-t", "--target"], &["-k", "--keep-mode"]],
            Vec::new(),
            line,
            text,
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

        // Snapshot the group before connecting so we can report which hosts were
        // actually added vs. skipped/failed to connect.
        let before: std::collections::HashSet<String> =
            session.targets().names().into_iter().collect();

        if hosts.is_empty() {
            // No -t: resolve the report's testplatforms and connect the hosts.
            session.add_testplatform_hosts().await;
            let mut added: Vec<String> = session
                .targets()
                .names()
                .into_iter()
                .filter(|n| !before.contains(n))
                .collect();
            added.sort();
            if added.is_empty() {
                session
                    .display
                    .println("no reference hosts resolved/connected");
            } else {
                session
                    .display
                    .println(&format!("added {}", added.join(", ")));
            }
            Ok(())
        } else {
            // Explicit -t: connect and add each named host.
            session.add_named_hosts(hosts.clone()).await;
            let after: std::collections::HashSet<String> =
                session.targets().names().into_iter().collect();
            let mut added: Vec<String> = hosts
                .iter()
                .filter(|h| !before.contains(*h) && after.contains(*h))
                .cloned()
                .collect();
            let mut skipped: Vec<String> = hosts
                .iter()
                .filter(|h| !added.contains(h))
                .cloned()
                .collect();
            added.sort();
            skipped.sort();

            // A requested `-t` host that never connected is a hard failure: with
            // zero added, surface an error so the caller (and, via MCP, the LLM)
            // is not told a phantom success.
            if added.is_empty() {
                return Err(CommandError::Other(format!(
                    "could not connect any requested host: {}",
                    skipped.join(", ")
                )));
            }
            let mut msg = format!("added {}", added.join(", "));
            if !skipped.is_empty() {
                msg.push_str(&format!("; skipped {}", skipped.join(", ")));
            }
            session.display.println(&msg);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::TargetState;
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
        session.metadata_mut().base_mut().testplatforms =
            tps.iter().map(|s| (*s).to_owned()).collect();
    }

    /// Pre-adds an already-connected (mock) target named `host` to the active
    /// group, so a subsequent `add_host -t host` sees `connect` short-circuit.
    fn add_mock_host(session: &mut Session, host: &str) {
        let conn = MockConnection::new(host).with_default(CommandLog::new("", "ok", "", 0, 0));
        let target = Target::with_connection(host, TargetState::Enabled, Box::new(conn));
        session.targets_mut().add(target);
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(AddHost.name(), "add_host");
        assert_eq!(AddHost.scope(), Scope::Fanout);
    }

    #[test]
    fn complete_offers_flags_and_templates_but_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = AddHost.complete(&session, "", "add_host ");
        assert!(out.contains(&"-t".to_owned()), "{out:?}");
        assert!(out.contains(&"-k".to_owned()), "{out:?}");
        assert!(out.contains(&"--keep-mode".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        // add_host connects *new* hosts, so it does not offer already-loaded ones.
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    /// Behavior change (bead w7w4.9): a requested `-t` host that cannot connect
    /// is a hard failure, not a silent skip. When *zero* requested hosts are
    /// added, `call` returns `Err` so the MCP result is an error rather than a
    /// phantom success. (Previously this asserted an `Ok`-skip.)
    #[tokio::test]
    async fn named_hosts_that_cannot_connect_are_error() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("unreachable.invalid")));
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
    /// a *different* unreachable host: the failed connect errors (zero added)
    /// but the existing member is untouched (one bad host never disturbs the
    /// group).
    #[tokio::test]
    async fn existing_mock_host_survives_a_failed_add() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        add_mock_host(&mut session, "h2");
        let before = session.targets().len();
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        assert_eq!(session.targets().len(), before);
        assert!(session.targets().names().contains(&"h2".to_owned()));
    }

    /// Running `add_host` in the automatic workflow switches to manual — the
    /// mode switch happens before the connect, so it stands even when the
    /// unreachable `-t` host makes the command error.
    #[tokio::test]
    async fn switches_auto_workflow_to_manual() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// `--keep-mode` preserves the automatic workflow.
    #[tokio::test]
    async fn keep_mode_preserves_auto_workflow() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable.invalid", "-k"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Auto);
    }

    /// A manual workflow is left untouched (no spurious downgrade path).
    #[tokio::test]
    async fn manual_workflow_is_left_untouched() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Manual);
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// Without `-t`, add_host resolves the report's testplatforms via the
    /// offline `path` refhosts resolver and connects the resulting hosts. The
    /// resolved fixture hosts are not mock-backed, so they fail to connect and
    /// are skipped — but the testplatform-resolution path is driven end to end.
    #[tokio::test]
    async fn no_target_resolves_testplatforms() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        set_testplatforms(&mut session, &["base=sles(major=15,minor=5);arch=[x86_64]"]);
        let args = matches(&AddHost, &[]);
        AddHost.call(&mut session, &args).await.unwrap();
        // Resolution ran (no panic); unreachable fixture hosts were skipped, so
        // the group is unchanged.
        assert_eq!(session.targets().len(), 1);
        // Zero new hosts connected: the display still gets a non-empty line so
        // the MCP result is never empty.
        assert!(
            buf.contents()
                .contains("no reference hosts resolved/connected"),
            "{:?}",
            buf.contents()
        );
    }
}
