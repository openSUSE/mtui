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

/// Resolves the per-template close budget. In tests it is overridable (via
/// `tests::set_close_timeout`) so the straggler path can be exercised without
/// waiting the full budget; in production it is always [`HOST_CLOSE_TIMEOUT`].
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
/// It accepts an optional positional
/// `bootarg ∈ {reboot, poweroff}` and, on quit, for every connected host group
/// ([`Session::take_teardown_units`](crate::Session::take_teardown_units) — every
/// loaded template *and* hosts attached while nothing was loaded):
/// releases the report's host-arbitration pool claims (in-process arbiter
/// ownership + remote pool locks) then closes its host group — rebooting
/// (`reboot`), powering off (`poweroff` → shell `halt`), or simply
/// disconnecting when no bootarg is given. Each group's close runs under
/// [`HOST_CLOSE_TIMEOUT`] so a hung host never blocks exit; a host that fails to disconnect
/// is named (`failed to disconnect from <host>: <err>`) and a host still
/// disconnecting at the budget is named as a straggler
/// (`still disconnecting from <host> after <secs> seconds`). Afterwards it
/// flips
/// [`Session::request_exit`](crate::Session::request_exit) and returns `Ok(())`
/// (the REPL checks [`should_exit`](crate::Session::should_exit) after each line
/// and breaks its loop).
///
/// It runs exactly once ([`Scope::Single`]) and is REPL-only — on the MCP
/// deny-list (a headless client has no session loop to quit). The aliases
/// `exit`/`EOF` dispatch to this same command, so `exit reboot` and the `Ctrl-D`
/// path inherit the bootarg + close behaviour.
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
        // Every connected host group, not just the loaded templates' — see
        // `Session::take_teardown_units`.
        for entry in session.take_teardown_units() {
            // Lock the unit to tear it down; uncontended while the outer
            // session mutex still serialises dispatch (steps 1-3).
            let mut report = entry.lock().await;
            // Release arbiter ownership + remote pool locks before
            // disconnecting (best-effort; a no-op without pooling).
            report.release_pool_claims().await;

            // Snapshot the group's hostnames so a straggler (the whole close
            // exceeding the budget) can still be named per host.
            let hosts = report.base_mut().targets.names();

            // Close the group under a per-unit budget: reboot / halt / plain
            // disconnect. Never let a hung host block exit.
            let close = report.base_mut().targets.close(action.as_deref());
            match tokio::time::timeout(timeout, close).await {
                Ok(outcomes) => {
                    // Name every host that failed to disconnect.
                    for (host, outcome) in &outcomes {
                        if let Err(e) = outcome {
                            tracing::warn!("failed to disconnect from {host}: {e}");
                        }
                    }
                }
                Err(_) => {
                    // Budget expired: the group is a straggler. Name each host.
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

    /// Test-only override for [`close_timeout`], in milliseconds. `u64::MAX`
    /// means "use the production [`HOST_CLOSE_TIMEOUT`]". Serialised by
    /// [`CLOSE_TIMEOUT_LOCK`] so a shrunk budget never leaks into a concurrent
    /// test.
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
        // clap enforces the choice set at parse time.
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
        // Two loaded templates, each with hosts. `quit` (no arg) closes both.
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

    /// Builds a locked target over a probe that stays observable after the
    /// target takes ownership (`MockConnection` shares its state across clones).
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

    /// Direction (a): hosts attached while **nothing is loaded** live in the
    /// null-report group, which no registry walk can reach. `quit` must still
    /// disconnect them and release their remote operation lock.
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
    ///
    /// Also pins the CHANGELOG's `quit reboot` claim (#478): a bootarg must
    /// reach the null-group host, not just the template one. Planting a host
    /// in *both* groups matters here — a null-only probe could not tell "the
    /// bootarg reached the null unit" from "nobody got it". Mutation: `if
    /// is_null { None } else { action }` at `quit.rs:109` must turn the null
    /// assertion red while leaving the template one green.
    #[tokio::test]
    async fn quit_closes_template_and_null_group_hosts() {
        let (tmpl_target, tmpl_probe) = locked_target("t1").await;
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", vec![tmpl_target]);

        // Plant a host in the sentinel's group: release the per-call guard so
        // `targets_mut()` falls back to the null report, then restore it so quit
        // runs from the realistic guard-held state.
        session.release_active_guard();
        let (null_target, null_probe) = locked_target("n1").await;
        session.targets_mut().add(null_target);
        session.refresh_active_guard();

        // Arm the assertion: `lock("")` writes over SFTP, not
        // `fire_and_forget`, so both probes start with no fired commands.
        assert!(tmpl_probe.fired_commands().is_empty());
        assert!(null_probe.fired_commands().is_empty());

        let args = matches(&Quit, &["reboot"]);
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
        assert_eq!(
            tmpl_probe.fired_commands(),
            vec!["reboot"],
            "t1: template host rebooted"
        );
        assert_eq!(
            null_probe.fired_commands(),
            vec!["reboot"],
            "n1: null-group host rebooted too"
        );
    }

    /// Sibling of the above for `poweroff`, which maps to the shell `halt`
    /// command — its own contract, and a separate mutant from `reboot`'s.
    #[tokio::test]
    async fn quit_poweroff_closes_template_and_null_group_hosts() {
        let (tmpl_target, tmpl_probe) = locked_target("t1").await;
        let (mut session, _buf) = session_with_targets("SUSE:Maintenance:1:1", vec![tmpl_target]);

        session.release_active_guard();
        let (null_target, null_probe) = locked_target("n1").await;
        session.targets_mut().add(null_target);
        session.refresh_active_guard();

        assert!(tmpl_probe.fired_commands().is_empty());
        assert!(null_probe.fired_commands().is_empty());

        let args = matches(&Quit, &["poweroff"]);
        Quit.call(&mut session, &args).await.unwrap();

        assert_eq!(
            tmpl_probe.fired_commands(),
            vec!["halt"],
            "t1: template host powered off"
        );
        assert_eq!(
            null_probe.fired_commands(),
            vec!["halt"],
            "n1: null-group host powered off too"
        );
    }

    #[tokio::test]
    async fn quit_names_host_that_fails_to_disconnect() {
        // One host fails to close, one closes cleanly. `quit` still sets exit
        // (best-effort) and the failing host surfaces an `Err` in the group's
        // teardown outcome map — the same map `quit` names failures from.
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

        // Re-close the group directly to assert the per-host outcome `quit`
        // reads: the failing host is named with an `Err`, the healthy one `Ok`.
        // (The first close already tore the group down; a second close on the
        // now-closed mocks still reports the scripted failure deterministically.)
        // `quit` released the active handle, so re-lock the entry directly to
        // assert the per-host outcome it read.
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
        // A wedged host whose close never returns must not block quit past the
        // (shrunk) per-template budget; quit still sets exit and names the
        // straggler. Serialise the timeout override so it does not leak.
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
        // A generous outer bound: if the per-template budget were ignored this
        // would hang, so cap the whole call well under the 45s production value.
        tokio::time::timeout(Duration::from_secs(5), Quit.call(&mut session, &args))
            .await
            .expect("quit must return despite the wedged host")
            .expect("quit ok");
        assert!(session.should_exit());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "quit returned within the shrunk budget, not the 45s production value"
        );

        // Let the abandoned close unwind and reset the override for other tests.
        gate.notify_waiters();
        CLOSE_TIMEOUT_MS.store(u64::MAX, Ordering::SeqCst);
    }
}
