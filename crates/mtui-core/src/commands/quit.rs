//! The `quit` command.

use std::time::Duration;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::{HOST_CLOSE_TIMEOUT, Session};

/// The two accepted boot actions, mirrored onto the CLI positional and reused
/// for completion.
const BOOT_ACTIONS: [&str; 2] = ["reboot", "poweroff"];

/// The per-template close budget: always [`HOST_CLOSE_TIMEOUT`] in production,
/// overridable in tests so the straggler path need not wait it out.
#[cfg(not(test))]
fn close_timeout() -> Duration {
    HOST_CLOSE_TIMEOUT
}
#[cfg(test)]
fn close_timeout() -> Duration {
    tests::close_timeout_override()
}

/// Disconnects from all hosts and exits the interactive session.
///
/// For every connected host group
/// ([`Session::take_teardown_units`](crate::Session::take_teardown_units) —
/// every loaded template *and* hosts attached while nothing was loaded) it
/// releases the pool claims (arbiter ownership + remote pool locks), then closes
/// the group per the optional positional `bootarg ∈ {reboot, poweroff}`
/// (`poweroff` → shell `halt`), or just disconnects. Each close runs under
/// [`HOST_CLOSE_TIMEOUT`] so a hung host never blocks exit, and one that fails
/// or is still disconnecting at the budget is named. It then flips
/// [`Session::request_exit`](crate::Session::request_exit), which the REPL reads
/// via [`should_exit`](crate::Session::should_exit) to break its loop.
///
/// [`Scope::Single`] and REPL-only — on the MCP deny-list, a headless client
/// having no session loop to quit. The aliases `exit`/`EOF` dispatch here, so
/// `exit reboot` and `Ctrl-D` inherit the bootarg + close behaviour.
pub struct Quit;

#[async_trait]
impl Command for Quit {
    fn name(&self) -> &'static str {
        "quit"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["exit", "EOF"]
    }

    fn about(&self) -> Option<&'static str> {
        Some("Disconnect from all hosts and exit (optionally reboot/poweroff).")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("bootarg")
                .num_args(0..=1)
                .value_name("BOOTARG")
                .value_parser(clap::builder::PossibleValuesParser::new(BOOT_ACTIONS))
                .help("reboot or poweroff refhosts on exit"),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        BOOT_ACTIONS
            .iter()
            .filter(|c| c.starts_with(text))
            .map(|c| (*c).to_owned())
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let action: Option<String> = args.get_one::<String>("bootarg").cloned();

        let timeout = close_timeout();
        for entry in session.take_teardown_units() {
            // Uncontended: the outer session mutex still serialises dispatch.
            let mut report = entry.lock().await;
            // Best-effort, and a no-op without pooling.
            report.release_pool_claims().await;

            // Snapshotted before the close so a straggling group can still be
            // named per host.
            let hosts = report.base_mut().targets.names();

            let close = report.base_mut().targets.close(action.as_deref());
            match tokio::time::timeout(timeout, close).await {
                Ok(outcomes) => {
                    for (host, outcome) in &outcomes {
                        if let Err(e) = outcome {
                            tracing::warn!("failed to disconnect from {host}: {e}");
                        }
                    }
                }
                Err(_) => {
                    let secs = timeout.as_secs();
                    for host in &hosts {
                        tracing::warn!("still disconnecting from {host} after {secs} seconds");
                    }
                }
            }
        }

        session.request_exit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use mtui_hosts::{MockConnection, TARGET_LOCK_PATH, Target};
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::commands::testkit::{
        empty_session, fake_report, matches, session_with_hosts, session_with_targets,
    };

    /// Override for [`close_timeout`] in milliseconds; `u64::MAX` means the
    /// production [`HOST_CLOSE_TIMEOUT`]. Serialised by [`CLOSE_TIMEOUT_LOCK`] so
    /// a shrunk budget never leaks into a concurrent test.
    static CLOSE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static CLOSE_TIMEOUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn close_timeout_override() -> Duration {
        match CLOSE_TIMEOUT_MS.load(Ordering::SeqCst) {
            u64::MAX => HOST_CLOSE_TIMEOUT,
            ms => Duration::from_millis(ms),
        }
    }

    #[test]
    fn name_aliases_and_single_scope() {
        assert_eq!(Quit.name(), "quit");
        assert_eq!(Quit.aliases(), &["exit", "EOF"]);
        assert_eq!(Quit.scope(), Scope::Single);
    }

    #[test]
    fn completes_boot_actions() {
        let (session, _buf) = empty_session();
        assert_eq!(Quit.complete(&session, "", ""), vec!["reboot", "poweroff"]);
        assert_eq!(Quit.complete(&session, "re", ""), vec!["reboot"]);
        assert_eq!(Quit.complete(&session, "po", ""), vec!["poweroff"]);
        assert!(Quit.complete(&session, "x", "").is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_bootarg() {
        let cmd = Quit.configure(clap::Command::new("quit"));
        assert!(cmd.try_get_matches_from(["quit", "restart"]).is_err());
    }

    #[tokio::test]
    async fn quit_requests_exit_without_bootarg() {
        let (mut session, _buf) = empty_session();
        assert!(!session.should_exit());
        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());
    }

    #[tokio::test]
    async fn quit_closes_all_loaded_templates_without_reboot() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h3"], "ok"));

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());
    }

    #[tokio::test]
    async fn quit_reboot_sets_exit() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Quit, &["reboot"]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());
    }

    #[tokio::test]
    async fn quit_poweroff_sets_exit() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Quit, &["poweroff"]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());
    }

    /// Builds a target whose mock connection is scripted with `build`.
    fn target_with(host: &str, build: impl FnOnce(MockConnection) -> MockConnection) -> Target {
        let conn = build(MockConnection::new(host));
        Target::with_connection(host, TargetState::Enabled, Box::new(conn))
    }

    /// A locked target over a probe that stays observable after the target takes
    /// ownership (`MockConnection` shares state across clones).
    async fn locked_target(host: &str) -> (Target, MockConnection) {
        let probe = MockConnection::new(host);
        let mut target =
            Target::with_connection(host, TargetState::Enabled, Box::new(probe.clone()));
        target.lock("").await.expect("operation lock taken");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_some(),
            "{host}: fixture must arm the assertion — the remote lock file exists before quit"
        );
        (target, probe)
    }

    /// Hosts attached while nothing is loaded live in the null-report group,
    /// which no registry walk reaches; `quit` must still disconnect them and
    /// release their remote operation lock.
    #[tokio::test]
    async fn quit_closes_hosts_attached_with_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        assert!(
            !session.metadata().is_loaded(),
            "fixture must reach the no-report state add_host writes into"
        );
        let (target, probe) = locked_target("n1").await;
        session.targets_mut().add(target);

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();

        assert!(probe.is_closed(), "n1: connection closed by quit");
        assert!(
            probe.file_contents(TARGET_LOCK_PATH).is_none(),
            "n1: remote operation lock released by quit"
        );
        assert!(session.should_exit());
    }

    /// Both directions at once: widening teardown to the null group must not
    /// *replace* the registry walk — a template's hosts are still torn down.
    #[tokio::test]
    async fn quit_closes_template_and_null_group_hosts() {
        let (tmpl_target, tmpl_probe) = locked_target("t1").await;
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", vec![tmpl_target]);

        // Releasing the per-call guard makes `targets_mut()` fall back to the
        // null report; restoring it leaves quit in the realistic held state.
        session.release_active_guard();
        let (null_target, null_probe) = locked_target("n1").await;
        session.targets_mut().add(null_target);
        session.refresh_active_guard();

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();

        assert!(tmpl_probe.is_closed(), "t1: template host still closed");
        assert!(
            tmpl_probe.file_contents(TARGET_LOCK_PATH).is_none(),
            "t1: template host's lock still released"
        );
        assert!(null_probe.is_closed(), "n1: null-group host closed");
        assert!(
            null_probe.file_contents(TARGET_LOCK_PATH).is_none(),
            "n1: null-group host's lock released"
        );
    }

    #[tokio::test]
    async fn quit_names_host_that_fails_to_disconnect() {
        // A failing host must not stop quit setting exit.
        let targets = vec![
            target_with("good", |c| {
                c.with_default(CommandLog::new("", "ok", "", 0, 0))
            }),
            target_with("bad", MockConnection::with_failing_close),
        ];
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", targets);

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();
        assert!(session.should_exit());

        // `quit` released the active handle, so re-lock the entry to read the
        // outcome map it named failures from — a second close on the now-closed
        // mocks still reports the scripted failure deterministically.
        let entry = session
            .templates
            .handle("SUSE:Maintenance:1:1")
            .expect("report loaded");
        let mut report = entry.lock().await;
        let outcomes = report.base_mut().targets.close(None).await;
        assert!(outcomes["good"].is_ok());
        assert!(outcomes["bad"].is_err(), "failing host is named with Err");
    }

    #[tokio::test]
    async fn quit_returns_promptly_when_a_host_straggles() {
        // Serialised so the shrunk budget does not leak into another test.
        let _guard = CLOSE_TIMEOUT_LOCK.lock().await;
        CLOSE_TIMEOUT_MS.store(50, Ordering::SeqCst);

        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let targets = vec![target_with("wedged", {
            let gate = std::sync::Arc::clone(&gate);
            move |c| c.with_blocking_close(gate)
        })];
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", targets);

        let args = matches(&Quit, &[]);
        let start = std::time::Instant::now();
        // Well under the 45s production budget: ignoring the per-template one
        // would hang here.
        tokio::time::timeout(Duration::from_secs(5), Quit.call(&mut session, &args))
            .await
            .expect("quit must return despite the wedged host")
            .expect("quit ok");
        assert!(session.should_exit());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "quit returned within the shrunk budget, not the 45s production value"
        );

        gate.notify_waiters();
        CLOSE_TIMEOUT_MS.store(u64::MAX, Ordering::SeqCst);
    }
}
