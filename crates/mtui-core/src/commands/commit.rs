//! The `commit` command (commits the testing template to SVN).

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_testreport::{TokioSvnRunner, detect_system, svn_commit_testreport, system_info};

use super::support::complete_with_templates;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Commits the testing template working copy to SVN.
///
/// Ports upstream `mtui.commands.commit.Commit`. Run after testing to persist
/// the final template. With `-m/--msg` the given message is used; without it a
/// message is generated from the local system info (upstream reuses the export
/// footer via `system_info(..., prefix="committed from")`) so the commit is
/// non-interactive rather than opening `svn`'s editor. Requires a loaded report.
pub struct Commit;

#[async_trait]
impl Command for Commit {
    fn name(&self) -> &'static str {
        "commit"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Commits the testing template working copy to SVN.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("msg")
                .short('m')
                .long("msg")
                .action(ArgAction::Append)
                .num_args(1..)
                .value_name("MSG")
                .help("commit message"),
        )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_with_templates(session, &[&["-m", "--msg"]], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let checkout = session
            .metadata()
            .base()
            .report_wd()
            .map_err(|e| CommandError::Other(format!("no report loaded: {e}")))?;
        let install_logs = session.config.install_logs.clone();

        // -m tokens join into one message; without it, a generated system-info
        // message keeps the commit non-interactive (upstream behaviour).
        let msg: Vec<String> = match args
            .try_get_many::<String>("msg")
            .ok()
            .flatten()
            .map(|it| it.cloned().collect::<Vec<_>>())
        {
            Some(tokens) if !tokens.is_empty() => {
                vec!["-m".to_owned(), format!("\"{}\"", tokens.join(" "))]
            }
            _ => {
                let (distro, verid, kernel) = detect_system();
                let default = system_info(
                    &distro,
                    &verid,
                    &kernel,
                    &session.config.session_user,
                    "committed from",
                )
                .trim_end()
                .to_owned();
                vec!["-m".to_owned(), default]
            }
        };

        let runner = TokioSvnRunner;
        svn_commit_testreport(&runner, &checkout, &install_logs, &msg)
            .await
            .map_err(|e| CommandError::Other(format!("committing template failed: {e}")))?;
        session.display.println(&format!(
            "testreport committed: {}",
            session.metadata().fancy_report_url()
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn complete_offers_msg_flag_and_templates_no_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Commit.complete(&session, "", "commit ");
        assert!(
            out.contains(&"-m".to_owned()) && out.contains(&"--msg".to_owned()),
            "{out:?}"
        );
        assert!(out.contains(&"SUSE:Maintenance:1:1".to_owned()), "{out:?}");
        assert!(!out.contains(&"h1".to_owned()), "{out:?}");
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Commit.name(), "commit");
        assert_eq!(Commit.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn no_report_errors_before_shelling_out() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Commit, &[]);
        let err = Commit.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// A successful commit prints the committed report URL to the display so the
    /// MCP result is never empty.
    #[tokio::test]
    async fn success_prints_committed_url_to_display() {
        if std::process::Command::new("svn")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // svn not installed in this environment
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wc = tmp.path().join("wc");
        assert!(
            std::process::Command::new("svnadmin")
                .args(["create", repo.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let repo_url = format!("file://{}", repo.display());
        assert!(
            std::process::Command::new("svn")
                .args(["checkout", &repo_url, wc.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );

        // The commit adds the install-logs dir; create it in the working copy so
        // `svn add --force install_logs` succeeds.
        std::fs::create_dir_all(wc.join("install_logs")).unwrap();

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().path = Some(wc.join("metadata.json"));

        let args = matches(&Commit, &["-m", "test commit"]);
        Commit.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("testreport committed:"), "{out:?}");
    }
}
