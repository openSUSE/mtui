//! The `quit` command.

use std::time::Duration;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::commands::support::leftover_notices;
use crate::error::CommandResult;
use crate::session::{HOST_CLOSE_TIMEOUT, Session};

/// The two accepted boot actions, mirrored onto the CLI positional and reused
/// for completion.
const BOOT_ACTIONS: [&str; 2] = ["reboot", "poweroff"];

/// The per-template close budget; overridable in tests.
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
/// ([`Session::take_teardown_units`](crate::Session::take_teardown_units),
/// including hosts attached while nothing was loaded) it releases the pool
/// claims, then closes the group per the optional positional
/// `bootarg ∈ {reboot, poweroff}` (`poweroff` → shell `halt`), or just
/// disconnects. The release and the close each run under [`HOST_CLOSE_TIMEOUT`]
/// so a hung host never blocks exit, and a host whose release is unfinished, or
/// that fails or straggles, is named. The close retries a hung release, so one
/// unit can take twice the budget. It then flips
/// [`Session::request_exit`](crate::Session::request_exit).
///
/// A report whose document holds uncommitted edits is named in a warning, never
/// refused: the teardown must always be able to release locks.
/// Anything the session left on a host is warned about the same way, from
/// in-memory records only, so it adds no remote I/O to the bounded teardown.
///
/// [`Scope::Single`] and REPL-only (on the MCP deny-list). The aliases
/// `exit`/`EOF` dispatch here.
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

    fn reads_resolved_report(&self) -> bool {
        // Tears down its own teardown units, not the active report.
        false
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
            // Never blocks: refusing to quit would strand the locks below.
            if report.base().document_dirty {
                let rrid = report.base().rrid.as_ref().map(ToString::to_string);
                session.display.println(&format!(
                    "warning: uncommitted document edits for {} discarded",
                    rrid.as_deref().unwrap_or("the loaded report")
                ));
            }
            // Best-effort, and a no-op without pooling.
            for host in report.release_pool_claims(timeout).await {
                let secs = timeout.as_secs();
                tracing::warn!("pool-claim release on {host} unfinished after {secs} seconds");
            }

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
            for line in leftover_notices(&report.base().targets.leftovers()) {
                session.display.println(&line);
            }
        }

        session.request_exit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use mtui_hosts::{MockConnection, POOL_LOCK_PATH, TARGET_LOCK_PATH, Target};
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::commands::testkit::{
        empty_session, fake_report, log_capture, matches, session_with_hosts, session_with_targets,
        target_with,
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
    async fn quit_warns_about_uncommitted_document_edits_and_still_exits() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().document_dirty = true;

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents()
                .contains("warning: uncommitted document edits for SUSE:Maintenance:1:1 discarded"),
            "{:?}",
            buf.contents()
        );
        assert!(session.should_exit());
    }

    #[tokio::test]
    async fn quit_is_silent_when_no_document_edits_are_pending() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();

        assert!(
            !buf.contents().contains("uncommitted"),
            "{:?}",
            buf.contents()
        );
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

        // Releasing the per-call guard makes `targets_mut()` fall back to the
        // null report; restoring it leaves quit in the realistic held state.
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

    /// Both the null group and a template's group are torn down, so each
    /// gets a notice; the released host gets none.
    #[tokio::test]
    async fn quit_names_locks_left_on_hosts() {
        let mut down = target_with("t1", MockConnection::inactive);
        down.lock("").await.expect("operation lock taken");
        let (clean, _probe) = locked_target("t2").await;
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", vec![down, clean]);
        session.release_active_guard();
        let mut null_down = target_with("n1", MockConnection::inactive);
        null_down.lock("").await.expect("operation lock taken");
        session.targets_mut().add(null_down);
        session.refresh_active_guard();

        let args = matches(&Quit, &[]);
        Quit.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        for host in ["t1", "n1"] {
            assert!(
                out.contains(&format!(
                    "warning: {host}: operation lock not released (link down); unless the \
                     host reboots, it stays until it is older than [lock] stale_age or someone \
                     runs `unlock -f`"
                )),
                "{out}"
            );
        }
        assert!(!out.contains("t2:"), "t2 released its lock: {out}");
        assert!(session.should_exit());
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

    /// A pool claim the report's own RRID owns, in the lock wire format.
    fn own_pool_claim(rrid: &str) -> Vec<u8> {
        let me = mtui_config::Config::default().session_user;
        format!("1700000000:{me}:1:mtui pool {rrid} [{rrid}]").into_bytes()
    }

    #[tokio::test]
    async fn quit_bounds_a_pool_claim_release_that_hangs() {
        let _guard = CLOSE_TIMEOUT_LOCK.lock().await;
        CLOSE_TIMEOUT_MS.store(500, Ordering::SeqCst);

        let rrid = "SUSE:Maintenance:1:1";
        // A delay under the budget, so a zero budget leaves this release unfinished.
        let healthy = MockConnection::new("healthy")
            .with_file(POOL_LOCK_PATH, own_pool_claim(rrid))
            .with_sftp_session_delay(Duration::from_millis(50));
        let mut healthy_target =
            Target::with_connection("healthy", TargetState::Enabled, Box::new(healthy.clone()));
        healthy_target.set_rrid(rrid);
        let dead = target_with("dead", |c| {
            c.with_sftp_session_delay(Duration::from_secs(3600))
        });
        let (mut session, _buf) = session_with_targets(rrid, vec![dead, healthy_target]);
        let claims = &mut session.metadata_mut().base_mut().pool_claims;
        claims.insert("dead".to_owned());
        claims.insert("healthy".to_owned());

        log_capture::start();
        let args = matches(&Quit, &[]);
        // Catches a release with no budget: quit then never returns.
        tokio::time::timeout(Duration::from_secs(5), Quit.call(&mut session, &args))
            .await
            .expect("quit must return despite the hung release")
            .expect("quit ok");
        let logged: Vec<String> = log_capture::take()
            .into_iter()
            .filter_map(|c| c.message)
            .collect();
        CLOSE_TIMEOUT_MS.store(u64::MAX, Ordering::SeqCst);

        assert!(session.should_exit());
        // Catches a teardown that skips the close after a timed-out release.
        assert!(healthy.is_closed(), "healthy: still closed");
        assert!(
            logged.contains(&"still disconnecting from dead after 0 seconds".to_owned()),
            "dead: the close was still attempted: {logged:?}"
        );
        // Exact lines: catches naming no host, every claimed host, a healthy one
        // a serial release never reached, or a zero budget.
        let releasing: Vec<&String> = logged
            .iter()
            .filter(|m| m.starts_with("pool-claim release"))
            .collect();
        assert_eq!(
            releasing,
            vec!["pool-claim release on dead unfinished after 0 seconds"],
            "only the hung host is named"
        );
    }
}
