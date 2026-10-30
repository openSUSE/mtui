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
/// A manual action, so an automatic workflow is moved to manual unless
/// `-k`/`--keep-mode`. With `-t`/`--target` each named host is connected and
/// added to the active report's group (`Session::add_named_hosts`); without it
/// the report's testplatforms are resolved through the refhosts factory and the
/// results connected (`Session::add_testplatform_hosts`). Only the workflow is
/// mutated — refreshing the prompt string is the REPL's concern.
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
        Scope::Explicit
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
        // An automatic workflow here almost certainly means the switch to manual
        // was forgotten. `set_workflow` is a no-op without an active guard, so it
        // gates the log: the message must describe a switch that landed.
        let keep_mode = args.get_flag("keep_mode");
        if session.metadata().workflow() == Workflow::Auto
            && !keep_mode
            && session.set_workflow(Workflow::Manual)
        {
            info!("add_host: switching from automatic to manual workflow");
        }

        let hosts: Vec<String> = args
            .get_many::<String>("target")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();

        // So added hosts can be told apart from skipped/failed ones afterwards.
        let before: std::collections::HashSet<String> =
            session.targets().names().into_iter().collect();

        if hosts.is_empty() {
            let unserved = session.add_testplatform_hosts().await;
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
            for reason in &unserved {
                session
                    .display
                    .println(&session.display.yellow(&reason.render()));
            }
            Ok(())
        } else {
            let refused = session.add_named_hosts(hosts.clone()).await;
            let after: std::collections::HashSet<String> =
                session.targets().names().into_iter().collect();
            let mut added: Vec<String> = hosts
                .iter()
                .filter(|h| !before.contains(*h) && after.contains(*h))
                .cloned()
                .collect();
            let refused_names: std::collections::HashSet<&String> =
                refused.iter().map(|(h, _)| h).collect();
            let mut skipped: Vec<String> = hosts
                .iter()
                .filter(|h| !added.contains(h) && !refused_names.contains(h))
                .cloned()
                .collect();
            added.sort();
            skipped.sort();

            // Zero added must be an error, or the caller (and via MCP the LLM)
            // is told a phantom success.
            if added.is_empty() {
                // The error names every requested host with its reason, so
                // no per-host display line here: with nothing added the
                // lines would duplicate the error (MCP carries both stdout
                // and stderr).
                let mut missing: Vec<String> = skipped;
                missing.extend(
                    refused
                        .iter()
                        .map(|(h, failure)| format!("{h} ({})", failure.render())),
                );
                missing.sort();
                return Err(CommandError::Other(format!(
                    "could not connect any requested host: {}",
                    missing.join(", ")
                )));
            }
            // Partial success: each refused host names its holder on its own
            // display line instead of vanishing into the skipped list.
            for (host, failure) in &refused {
                session.display.println(
                    &session
                        .display
                        .yellow(&format!("skipped {host}: {}", failure.render())),
                );
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

    /// Path to the offline refhosts fixture (no network).
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

    /// Pre-adds a connected mock target, so a later `add_host -t host` sees
    /// `connect` short-circuit.
    fn add_mock_host(session: &mut Session, host: &str) {
        let conn = MockConnection::new(host).with_default(CommandLog::new("", "ok", "", 0, 0));
        let target = Target::with_connection(host, TargetState::Enabled, Box::new(conn));
        session.targets_mut().add(target);
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(AddHost.name(), "add_host");
        assert_eq!(AddHost.scope(), Scope::Explicit);
    }

    #[test]
    fn complete_offers_flags_and_templates_but_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = AddHost.complete(&session, "", "add_host ");
        assert!(out.contains(&"-t".to_owned()), "{out:?}");
        assert!(out.contains(&"-k".to_owned()), "{out:?}");
        assert!(out.contains(&"--keep-mode".to_owned()), "{out:?}");
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        // It connects *new* hosts, so already-loaded ones are not offered.
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    /// A requested `-t` host that cannot connect is a hard failure, not a silent
    /// skip: zero added means `Err`, so the MCP result is an error rather than a
    /// phantom success.
    #[tokio::test]
    async fn named_hosts_that_cannot_connect_are_error() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        let args = matches(&AddHost, &["-t", "unreachable.invalid"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("unreachable.invalid")));
        assert_eq!(session.targets().len(), 1);
        assert!(
            !session
                .targets()
                .names()
                .contains(&"unreachable.invalid".to_owned())
        );
    }

    /// One bad host never disturbs the group: an `add_host` of a different,
    /// unreachable host errors but leaves the existing member untouched.
    ///
    /// Each connect-failure test below names its own unresolvable host:
    /// `templates.add` wires the process-global arbiter, so one shared
    /// hostname would arbitrate across parallel tests.
    #[tokio::test]
    async fn existing_mock_host_survives_a_failed_add() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        add_mock_host(&mut session, "h2");
        let before = session.targets().len();
        let args = matches(&AddHost, &["-t", "unreachable-survivor.invalid"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
        assert_eq!(session.targets().len(), before);
        assert!(session.targets().names().contains(&"h2".to_owned()));
    }

    /// The mode switch happens before the connect, so it stands even when the
    /// unreachable `-t` host makes the command error.
    #[tokio::test]
    async fn switches_auto_workflow_to_manual() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable-switch.invalid"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// `--keep-mode` preserves the automatic workflow.
    #[tokio::test]
    async fn keep_mode_preserves_auto_workflow() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        session.set_workflow(Workflow::Auto);
        let args = matches(&AddHost, &["-t", "unreachable-keep.invalid", "-k"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Auto);
    }

    /// A manual workflow is left untouched.
    #[tokio::test]
    async fn manual_workflow_is_left_untouched() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        session.set_workflow(Workflow::Manual);
        let args = matches(&AddHost, &["-t", "unreachable-manual.invalid"]);
        let _ = AddHost.call(&mut session, &args).await;
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
    }

    /// Drives the testplatform-resolution path end to end against the offline
    /// `path` resolver. The fixture hosts are not mock-backed, so they fail to
    /// connect and are skipped.
    #[tokio::test]
    async fn no_target_resolves_testplatforms() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        set_testplatforms(&mut session, &["base=sles(major=15,minor=5);arch=[x86_64]"]);
        let args = matches(&AddHost, &[]);
        AddHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.targets().len(), 1);
        // The display still gets a line, so the MCP result is never empty.
        assert!(
            buf.contents()
                .contains("no reference hosts resolved/connected"),
            "{:?}",
            buf.contents()
        );
        // And says why: every candidate of the slot was dialled and refused.
        assert!(
            buf.contents()
                .contains("unserved slot sles 15-5 x86_64: no candidate connected ("),
            "{:?}",
            buf.contents()
        );
        assert!(
            buf.contents().contains(": unreachable"),
            "{:?}",
            buf.contents()
        );
    }

    /// A `-t` host held by another template names its holder instead of
    /// attaching silently; with nothing else attached the call still errors.
    #[tokio::test]
    async fn named_host_held_by_another_template_names_its_holder() {
        use mtui_hosts::{HostArbiter, Owner};

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        let other: Owner = ("reg".to_owned(), "SUSE:Maintenance:2:2".to_owned());
        {
            let base = session.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner);
        }
        assert!(arbiter.try_acquire("h2", &other));

        let args = matches(&AddHost, &["-t", "h2"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        let msg = match &err {
            CommandError::Other(m) => m.clone(),
            other => panic!("expected refusal, got {other:?}"),
        };
        assert!(
            msg.contains("could not connect any requested host"),
            "{err:?}"
        );
        assert!(
            msg.contains("h2") && msg.contains("SUSE:Maintenance:2:2"),
            "the error names the refused host and its holder: {err:?}"
        );
        assert!(
            buf.contents().is_empty(),
            "with nothing added the error carries the refusal; no duplicate display line: {:?}",
            buf.contents()
        );
        assert_eq!(session.targets().len(), 1);
    }

    /// A `-t` host whose remote pool claim another template holds is refused
    /// with its holder named and nothing attaches: a fresh remote claim
    /// blocks `-t` with no override on that template. The holding template's
    /// `unlock --pool --force [host]` clears the claim and unblocks a retry.
    #[tokio::test]
    async fn named_host_with_remote_claim_is_blocked_naming_its_holder() {
        use mtui_hosts::{HostArbiter, Owner, POOL_LOCK_PATH};

        use crate::commands::HostsUnlock;
        use crate::commands::testkit::session_with_targets;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        use_path_refhosts(&mut session);
        let arbiter: &'static HostArbiter = Box::leak(Box::new(HostArbiter::new()));
        let owner: Owner = ("reg".to_owned(), "SUSE:Maintenance:1:1".to_owned());
        {
            let base = session.metadata_mut().base_mut();
            base.arbiter = Some(arbiter);
            base.owner = Some(owner);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let foreign =
            format!("{now}:otheruser:99:mtui pool SUSE:Maintenance:2:2 [bob]").into_bytes();
        // Kept by handle: the holding template below dials through a clone
        // sharing the same mock files, so its unlock is observable here.
        let holder_conn =
            MockConnection::new("remote-h2").with_file(POOL_LOCK_PATH, foreign.clone());
        session.inject_mock_connection("remote-h2", holder_conn.clone());

        let args = matches(&AddHost, &["-t", "remote-h2"]);
        let err = AddHost.call(&mut session, &args).await.unwrap_err();
        let msg = match &err {
            CommandError::Other(m) => m.clone(),
            other => panic!("expected refusal, got {other:?}"),
        };
        assert!(
            msg.contains("remote-h2") && msg.contains("SUSE:Maintenance:2:2"),
            "the error names the blocked host and its holder: {err:?}"
        );
        assert!(
            buf.contents().is_empty(),
            "with nothing added the error carries the refusal; no duplicate display line: {:?}",
            buf.contents()
        );
        assert_eq!(session.targets().len(), 1);

        // The refused template cannot clear the claim itself: `unlock`
        // resolves names against its own group, which the refused host never
        // joined.
        let unlock_args = matches(&HostsUnlock, &["-p", "-f", "-t", "remote-h2"]);
        let unlock_err = HostsUnlock
            .call(&mut session, &unlock_args)
            .await
            .unwrap_err();
        assert!(
            matches!(&unlock_err, CommandError::Other(m) if m.contains("remote-h2") && m.contains("not connected")),
            "{unlock_err:?}"
        );
        assert!(holder_conn.file_contents(POOL_LOCK_PATH).is_some());

        // The documented remedy runs where the host is attached: the holding
        // template force-removes the claim, and the retry attaches.
        let mut holder_target = Target::with_connection(
            "remote-h2",
            TargetState::Enabled,
            Box::new(holder_conn.clone()),
        );
        holder_target.set_rrid("SUSE:Maintenance:2:2");
        let (mut holder, holder_buf) =
            session_with_targets("SUSE:Maintenance:2:2", vec![holder_target]);
        let holder_args = matches(&HostsUnlock, &["-p", "-f", "-t", "remote-h2"]);
        HostsUnlock.call(&mut holder, &holder_args).await.unwrap();
        assert!(
            holder_buf
                .contents()
                .contains("remote-h2: pool claim removed"),
            "{:?}",
            holder_buf.contents()
        );
        assert!(holder_conn.file_contents(POOL_LOCK_PATH).is_none());

        let retry_args = matches(&AddHost, &["-t", "remote-h2"]);
        AddHost.call(&mut session, &retry_args).await.unwrap();
        assert!(
            session.targets().names().contains(&"remote-h2".to_owned()),
            "the retry attaches once the holder clears the claim"
        );
    }
}
