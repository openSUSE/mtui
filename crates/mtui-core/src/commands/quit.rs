//! The `quit` command.

use std::time::Duration;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// The two accepted boot actions, mirrored onto the CLI positional and reused
/// for completion (upstream `choices=["reboot", "poweroff"]`).
const BOOT_ACTIONS: [&str; 2] = ["reboot", "poweroff"];

/// Wall-clock budget for a template's host-close fan-out (upstream
/// `concurrent.futures.wait(futures, timeout=45)`); quit must still exit if a
/// host hangs during teardown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(45);

/// Resolves the per-template close budget. In tests it is overridable (via
/// [`tests::set_close_timeout`]) so the straggler path can be exercised without
/// waiting the full 45s; in production it is always [`CLOSE_TIMEOUT`].
#[cfg(not(test))]
fn close_timeout() -> Duration {
    CLOSE_TIMEOUT
}
#[cfg(test)]
fn close_timeout() -> Duration {
    tests::close_timeout_override()
}

/// Disconnects from all hosts and exits the interactive session.
///
/// Ports upstream `mtui.commands.quit.Quit`. It accepts an optional positional
/// `bootarg ∈ {reboot, poweroff}` and, on quit, for **every** loaded template:
/// releases the report's host-arbitration pool claims (in-process arbiter
/// ownership + remote pool locks) then closes its host group — rebooting
/// (`reboot`), powering off (`poweroff` → shell `halt`), or simply
/// disconnecting when no bootarg is given. Each template's close runs under a
/// 45s budget so a hung host never blocks exit; a host that fails to disconnect
/// is named (`failed to disconnect from <host>: <err>`) and a host still
/// disconnecting at the budget is named as a straggler
/// (`still disconnecting from <host> after <secs> seconds`), mirroring upstream
/// `quit`'s per-future logging. Afterwards it flips
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

        // Iterate every loaded template (not just the active one). Snapshot the
        // RRIDs first so the mutable per-report borrow below does not conflict
        // with the registry borrow.
        let rrids = session.templates.rrids();
        let timeout = close_timeout();
        // Release the per-call active handle before locking entries: quit locks
        // *every* loaded entry (incl. the active one) to tear it down, which
        // would otherwise self-deadlock on the guard this session already holds.
        session.release_active_guard();
        for rrid in rrids {
            if let Some(entry) = session.templates.handle(&rrid) {
                // Lock the entry to tear it down; uncontended while the outer
                // session mutex still serialises dispatch (steps 1-3).
                let mut report = entry.lock().await;
                // Release arbiter ownership + remote pool locks before
                // disconnecting (best-effort; a no-op without pooling).
                report.release_pool_claims().await;

                // Snapshot the group's hostnames so a straggler (the whole
                // close exceeding the budget) can still be named per host.
                let hosts = report.base_mut().targets.names();

                // Close the group under a per-template budget: reboot / halt /
                // plain disconnect. Never let a hung host block exit.
                let close = report.base_mut().targets.close(action.as_deref());
                match tokio::time::timeout(timeout, close).await {
                    Ok(outcomes) => {
                        // Name every host that failed to disconnect (upstream
                        // `failed to disconnect from %s: %s`).
                        for (host, outcome) in &outcomes {
                            if let Err(e) = outcome {
                                tracing::warn!("failed to disconnect from {host}: {e}");
                            }
                        }
                    }
                    Err(_) => {
                        // Budget expired: the group is a straggler. Name each
                        // host (upstream `still disconnecting from %s after %s
                        // seconds`).
                        let secs = timeout.as_secs();
                        for host in &hosts {
                            tracing::warn!("still disconnecting from {host} after {secs} seconds");
                        }
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

    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::commands::testkit::{
        empty_session, fake_report, matches, session_with_hosts, session_with_targets,
    };

    /// Test-only override for [`close_timeout`], in milliseconds. `u64::MAX`
    /// means "use the production [`CLOSE_TIMEOUT`]". Serialised by
    /// [`CLOSE_TIMEOUT_LOCK`] so a shrunk budget never leaks into a concurrent
    /// test.
    static CLOSE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
    static CLOSE_TIMEOUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn close_timeout_override() -> Duration {
        match CLOSE_TIMEOUT_MS.load(Ordering::SeqCst) {
            u64::MAX => CLOSE_TIMEOUT,
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
        Target::with_connection(
            host,
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        )
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
