//! The `whoami` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::Command;
use crate::error::CommandResult;
use crate::session::Session;

/// Displays the current user name and session PID.
///
/// Ports upstream `mtui.commands.whoami.Whoami`. The username comes from
/// [`Config::session_user`](mtui_config::Config) and the PID from the running
/// process; both form the session identity used for host locking and logging.
pub struct Whoami;

#[async_trait]
impl Command for Whoami {
    fn name(&self) -> &'static str {
        "whoami"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Displays the current user name and session PID.")
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let user = session.config.session_user.clone();
        let pid = std::process::id();
        session
            .display
            .println(&format!("User: {user}, app pid: {pid}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_is_whoami() {
        assert_eq!(Whoami.name(), "whoami");
    }

    #[tokio::test]
    async fn prints_user_and_pid() {
        let (mut session, buf) = empty_session();
        session.config.session_user = "alice".to_owned();
        let args = matches(&Whoami, &[]);
        Whoami.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("User: alice, app pid: "), "output: {out}");
        assert!(
            out.contains(&std::process::id().to_string()),
            "output: {out}"
        );
    }
}
