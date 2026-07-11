//! The `approve` command — approve the loaded update via OSC or Gitea.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::Osc;
use mtui_testreport::{HashCheck, TokioSvnRunner, svn_commit_testreport};

use crate::command::{Command, Scope};
use crate::commands::apicall::{PiAction, gitea_client, is_gitea_workflow, pi_autolock};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Approves the loaded update, dispatching to OSC or Gitea like the other
/// backend-API commands.
///
/// Ports upstream `mtui.commands.approve.Approve`. With `-r/--reviewer` the
/// reviewer is recorded in the testreport and the template is committed to SVN
/// *before* the approval; if either step fails the approval is aborted. On the
/// Gitea path a checkout-hash mismatch aborts the approval in non-interactive
/// mode (the interactive confirm prompt lands in Phase 6). Unlocks PI reference
/// hosts afterwards.
pub struct Approve;

#[async_trait]
impl Command for Approve {
    fn name(&self) -> &'static str {
        "approve"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Approves the loaded update, dispatching to OSC or Gitea like the other backend-API commands.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("group")
                .short('g')
                .long("group")
                .value_name("GROUP")
                .action(ArgAction::Append)
                .help("Group to approve (not valid for the Gitea workflow)"),
        )
        .arg(
            Arg::new("user")
                .short('u')
                .long("user")
                .value_name("USER")
                .default_value("")
                .help("User override for the Gitea workflow (Gitea only)"),
        )
        .arg(
            Arg::new("reviewer")
                .short('r')
                .long("reviewer")
                .value_name("NAME")
                .help("Record reviewer in the testreport, commit to SVN, then approve"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = ["-g", "--group", "-u", "--user", "-r", "--reviewer"]
            .iter()
            .filter(|f| f.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;

        // -r/--reviewer: record + commit before approving; abort on failure.
        if let Some(reviewer) = args.get_one::<String>("reviewer")
            && !record_reviewer(session, reviewer).await?
        {
            return Ok(());
        }

        let groups: Vec<String> = args
            .get_many::<String>("group")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        let user = args
            .get_one::<String>("user")
            .filter(|s| !s.is_empty())
            .cloned();

        if is_gitea_workflow(&rrid) {
            let gitea = gitea_client(session)?;
            // Any non-matching hash (a real mismatch, a missing token, or a
            // failed Gitea call) refuses a non-interactive approval, matching
            // the previous `!ok` guard.
            if let check @ (HashCheck::Mismatch { .. }
            | HashCheck::MissingToken
            | HashCheck::Failed(_)) = session.metadata().check_hash().await
                && !session.is_repl
            {
                let (expected, actual) = match check {
                    HashCheck::Mismatch { expected, actual } => (expected, actual),
                    _ => (String::new(), String::new()),
                };
                return Err(CommandError::Other(format!(
                    "GiteaPR hash differs from testreport ({expected} -> {actual}); \
                     refusing to approve non-interactively"
                )));
            }
            if let Err(e) = gitea.approve(user.as_deref()).await {
                tracing::error!("{e}");
            }
        } else {
            tracing::info!("Approving request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.approve(&groups)
                .await
                .map_err(|e| CommandError::Other(format!("osc approve failed: {e}")))?;
        }

        pi_autolock(session, PiAction::Unlock).await;
        Ok(())
    }
}

/// Records the reviewer and commits the testreport to SVN (upstream
/// `_record_reviewer`). Returns `true` when the approval should proceed.
async fn record_reviewer(session: &mut Session, name: &str) -> Result<bool, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        tracing::error!("Reviewer must be a non-empty string; not approving");
        return Ok(false);
    }

    if let Err(e) = session.templates.active_mut().set_reviewer(name) {
        tracing::error!("Failed to record reviewer, not approving: {e}");
        return Ok(false);
    }

    let checkout = session
        .metadata()
        .base()
        .report_wd()
        .map_err(|e| CommandError::Other(format!("no report loaded: {e}")))?;
    let install_logs = session.config.install_logs.clone();
    let msg = vec!["-m".to_owned(), format!("Add Test Plan Reviewer: {name}")];
    let runner = TokioSvnRunner;
    if let Err(e) = svn_commit_testreport(&runner, &checkout, &install_logs, &msg).await {
        tracing::error!("Failed to commit testreport to SVN, not approving: {e}");
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Approve.name(), "approve");
        assert_eq!(Approve.scope(), Scope::Fanout);
    }

    #[test]
    fn completion_offers_reviewer_flag() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Approve.complete(&session, "-r", "");
        assert_eq!(out, vec!["-r"]);
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn reviewer_with_no_template_path_aborts_gracefully() {
        // The report has no `path`, so set_reviewer fails → record_reviewer
        // returns false → approve aborts without error and never dispatches.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Approve, &["-r", "alice"]);
        Approve.call(&mut session, &args).await.unwrap();
        // Reviewer was NOT recorded (the write failed with no path).
        assert_eq!(session.metadata().base().reviewer, "");
    }

    #[tokio::test]
    async fn osc_dispatch_runs_for_maintenance_rrid() {
        // A Maintenance RRID routes to OSC. With no `osc` binary on PATH the
        // call fails, surfacing the OSC-branch error — which exercises the
        // non-gitea dispatch + error mapping without needing a real backend.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config.session_user = "tester".to_owned();
        let args = matches(&Approve, &["-g", "qam-sle"]);
        let res = Approve.call(&mut session, &args).await;
        // Either the osc call failed (no binary) → Err, or (unlikely in CI) it
        // succeeded; both mean the OSC branch executed.
        if let Err(e) = res {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc approve failed")));
        }
    }

    #[tokio::test]
    async fn gitea_hash_match_proceeds_to_approve() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // SLFO report → Gitea path; the fake report's check_hash reports a match,
        // so the guard passes and gitea.approve runs (its outcome is logged, not
        // returned), exercising the gitea success branch + pi_autolock(Unlock).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "requested_reviewers": [], "state": "open", "head": {"sha": "abc"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let (mut session, _buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.templates.active_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_token = "tok".to_owned();
        let args = matches(&Approve, &[]);
        Approve.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn gitea_hash_mismatch_aborts_headless() {
        // A SLFO report routes to Gitea; the fake report's check_hash reports a
        // match by default, so force a mismatch by giving no PR API URL path
        // that would error earlier. Instead we assert the non-interactive guard
        // via a report whose check_hash returns false.
        use async_trait::async_trait;
        use mtui_testreport::{TestReport, TestReportBase};
        use std::collections::HashMap;

        struct MismatchReport {
            base: TestReportBase,
        }
        #[async_trait]
        impl TestReport for MismatchReport {
            fn base(&self) -> &TestReportBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut TestReportBase {
                &mut self.base
            }
            fn id(&self) -> String {
                "SUSE:SLFO:1.2:5".to_owned()
            }
            fn parser(&self) -> HashMap<String, String> {
                HashMap::new()
            }
            fn update_repos_parser(&self) -> HashMap<mtui_types::SystemProduct, String> {
                HashMap::new()
            }
            fn list_update_commands(&self, _t: &mtui_hosts::HostsGroup) {}
            async fn check_hash(&self) -> HashCheck {
                HashCheck::Mismatch {
                    expected: "old".to_owned(),
                    actual: "new".to_owned(),
                }
            }
        }

        let (mut session, _buf) = empty_session();
        let mut base = TestReportBase::new(mtui_config::Config::default());
        base.rrid = "SUSE:SLFO:1.2:5".parse().ok();
        base.giteaprapi = Some("http://gitea.invalid/api".to_owned());
        session.config.gitea_token = "tok".to_owned();
        session.templates.add(Box::new(MismatchReport { base }));

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("hash differs")));
    }
}
