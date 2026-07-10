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

/// Wall-clock budget for the whole host-close fan-out (upstream
/// `concurrent.futures.wait(futures, timeout=45)`); quit must still exit if a
/// host hangs during teardown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(45);

/// Disconnects from all hosts and exits the interactive session.
///
/// Ports upstream `mtui.commands.quit.Quit`. It accepts an optional positional
/// `bootarg ∈ {reboot, poweroff}` and, on quit, for **every** loaded template:
/// releases the report's host-arbitration pool claims (in-process arbiter
/// ownership + remote pool locks) then closes its host group — rebooting
/// (`reboot`), powering off (`poweroff` → shell `halt`), or simply
/// disconnecting when no bootarg is given. The whole close phase runs under a
/// 45s budget so a hung host never blocks exit; afterwards it flips
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
        let close = async {
            for rrid in rrids {
                if let Some(report) = session.templates.get_mut(&rrid) {
                    // Release arbiter ownership + remote pool locks before
                    // disconnecting (best-effort; a no-op without pooling).
                    report.release_pool_claims().await;
                    // Close the group: reboot / halt / plain disconnect.
                    report.base_mut().targets.close(action.as_deref()).await;
                }
            }
        };

        // Best-effort: never let a hung host teardown block the exit.
        if tokio::time::timeout(CLOSE_TIMEOUT, close).await.is_err() {
            tracing::warn!("host close timed out after {CLOSE_TIMEOUT:?}; exiting anyway");
        }

        session.request_exit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

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
}
