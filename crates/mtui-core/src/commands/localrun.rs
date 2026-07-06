//! The `lrun` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Runs a command in the local shell.
///
/// Ports upstream `mtui.commands.localrun.LocalRun`. The command runs in mtui's
/// current working directory. When the session is interactive (a human at the
/// REPL) the child inherits the terminal so output streams live; under a
/// non-interactive session (MCP, headless callers) stdout and stderr are
/// captured and re-emitted through the display, and a non-zero exit is surfaced
/// as a command error carrying the real return code.
///
/// The positional tokens are re-quoted with `shlex::join` so a token containing
/// shell metacharacters keeps its quoting instead of being re-split by the
/// `sh -c` shell; shell operators (pipes, redirection) belong inside an explicit
/// `sh -c '...'`.
pub struct LocalRun;

#[async_trait]
impl Command for LocalRun {
    fn name(&self) -> &'static str {
        "lrun"
    }

    fn scope(&self) -> Scope {
        // Local execution never touches hosts or templates: run exactly once.
        Scope::Single
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("command")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .value_name("COMMAND")
                .help("command to run on local shell"),
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let tokens: Vec<String> = args
            .get_many::<String>("command")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        if tokens.is_empty() {
            return Err(CommandError::Other("Missing argument".to_owned()));
        }
        let cmd = shlex::try_join(tokens.iter().map(String::as_str))
            .map_err(|e| CommandError::Other(format!("invalid command: {e}")))?;

        if session.interactive {
            let status = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status()
                .await
                .map_err(|e| CommandError::Other(e.to_string()))?;
            if !status.success() {
                return Err(CommandError::Other(format!(
                    "local command failed: {status}"
                )));
            }
            return Ok(());
        }

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await
            .map_err(|e| CommandError::Other(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stdout.lines() {
            session.display.println(line);
        }
        for line in stderr.lines() {
            session.display.println(line);
        }
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(CommandError::Other(format!(
                "local command exited with code {code}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(LocalRun.name(), "lrun");
        assert_eq!(LocalRun.scope(), Scope::Single);
    }

    #[tokio::test]
    async fn missing_argument_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&LocalRun, &[]);
        let err = LocalRun.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m == "Missing argument"));
    }

    #[tokio::test]
    async fn captures_stdout_when_headless() {
        // A non-interactive session captures and re-emits child output.
        let (mut session, buf) = empty_session();
        assert!(!session.interactive);
        let args = matches(&LocalRun, &["printf", "hello"]);
        LocalRun.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("hello"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn nonzero_exit_propagates_headless() {
        let (mut session, _buf) = empty_session();
        let args = matches(&LocalRun, &["sh", "-c", "exit 3"]);
        let err = LocalRun.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("code 3")));
    }
}
