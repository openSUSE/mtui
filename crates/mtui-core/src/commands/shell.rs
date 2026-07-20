//! The `shell` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, select_names};
use crate::command::Command;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Opens an interactive shell on a reference host.
///
/// Ports upstream `mtui.commands.shell.Shell`. Attaching an interactive PTY to a
/// remote shell needs a controlling terminal, which only the Phase-6 `mtui`
/// binary owns; the command surface (name, args, host selection, completion) is
/// ported here so the registry and MCP synthesiser see it, but the runtime PTY
/// attach is deferred to Phase 6. Invoked headlessly it errors cleanly rather
/// than hanging. REPL-only — on the MCP deny-list.
pub struct Shell;

#[async_trait]
impl Command for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Opens an interactive shell on a reference host.")
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        // Upstream `shell` takes only `-t/--target` and opens an interactive
        // root shell per host; no command positional (strict parity, gap #5).
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .targets()
            .names()
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // Validate host selection so headless callers get the same argument
        // errors the REPL would, but the interactive PTY attach is Phase 6.
        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        Err(CommandError::Other(
            "interactive shell attach is not available in this mode (Phase 6 REPL only)".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_is_shell() {
        assert_eq!(Shell.name(), "shell");
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Shell, &[]);
        let err = Shell.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn headless_attach_errors_cleanly() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Shell, &["-t", "h1"]);
        let err = Shell.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("not available")));
    }

    #[test]
    fn complete_offers_host_names() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let candidates = Shell.complete(&session, "h", "shell h");
        assert_eq!(candidates, vec!["h1".to_owned(), "h2".to_owned()]);
    }
}
