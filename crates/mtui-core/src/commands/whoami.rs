//! The `whoami` command.

use async_trait::async_trait;
use clap::ArgMatches;

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Displays the current user name and session PID.
///
/// [`Config::session_user`](mtui_config::Config) plus the running process's PID
/// — together the session identity used for host locking and logging.
/// Session-level: runs once ([`Scope::Single`]) and takes no template flag.
pub struct Whoami;

#[async_trait]
impl Command for Whoami {
    fn name(&self) -> &'static str {
        "whoami"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Displays the current user name and session PID.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Session identity; no report involved.
        false
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
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

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

    /// Headless with several templates loaded, the session identity is printed
    /// once — not once per template under a banner (#597).
    #[tokio::test]
    async fn runs_once_headless_with_several_templates_loaded() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert!(!session.is_repl && session.templates.len() == 2);
        let args = matches(&Whoami, &[]);
        Whoami.run(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("User: ")).count(),
            1,
            "output: {out}"
        );
        assert!(!out.contains("=== "), "output: {out}");
    }
}
