//! The `approve` command — approve the loaded update via OSC or Gitea.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{Osc, Slack, is_ack_reaction};
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

        // Slack gate: when the site has opted into Slack review, an approval
        // requires an acknowledged review request. Runs before any state
        // changes so a refused approval leaves nothing half-done.
        slack_review_gate(session, &rrid).await?;

        // -r/--reviewer: record + commit before approving; abort on failure.
        if let Some(reviewer) = args.get_one::<String>("reviewer") {
            record_reviewer(session, reviewer).await?;
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
            gitea
                .approve(user.as_deref())
                .await
                .map_err(|e| CommandError::Other(format!("gitea approve failed: {e}")))?;
        } else {
            tracing::info!("Approving request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.approve(&groups)
                .await
                .map_err(|e| CommandError::Other(format!("osc approve failed: {e}")))?;
        }

        pi_autolock(session, PiAction::Unlock).await;
        session.display.println(&format!("approved {rrid}"));
        Ok(())
    }
}

/// Refuse the approval unless the update's Slack review request was acked.
///
/// Only engages when the site has opted into the integration
/// (`[slack] enabled = true`); with Slack off this is a no-op and `approve`
/// behaves exactly as it always has. Once on, the gate is deliberately strict,
/// because a gate with a per-invocation bypass flag is not a gate: turning it
/// off is a config change (`config set slack_enabled false`), which is
/// explicit and auditable rather than a habit that creeps into muscle memory.
///
/// Three things must hold, and each rules out a distinct way of approving
/// something nobody reviewed:
///
/// 1. A marker exists — otherwise no review was ever requested.
/// 2. The marked message still names this RRID — otherwise a marker copied
///    from another template (or a re-used message) would launder an approval
///    for an update nobody looked at.
/// 3. Someone other than the bot left an approving reaction.
///
/// Only `approve` is gated. `reject` is the conservative direction: blocking
/// it would strand an update that a reviewer wants stopped.
async fn slack_review_gate(
    session: &mut Session,
    rrid: &mtui_types::RequestReviewID,
) -> Result<(), CommandError> {
    if !session.config.slack_enabled {
        return Ok(());
    }

    let Some(marker) = session.metadata().base().slack_review.clone() else {
        return Err(CommandError::Other(format!(
            "Slack review is enabled but no review was requested for {rrid}; \
             run `request_review` first (or disable the gate with \
             `config set slack_enabled false`)"
        )));
    };

    let slack = Slack::new(&session.config)
        .map_err(|e| CommandError::Other(format!("could not check the Slack review: {e}")))?;
    let message = slack
        .get_message(&marker.channel, &marker.ts)
        .await
        .map_err(|e| {
            CommandError::Other(format!(
                "could not read the Slack review request for {rrid}, not approving: {e}"
            ))
        })?;

    // Bind the marker to this update: a message that does not name this RRID
    // is not this update's review, whatever the template claims.
    if !message.text.contains(&rrid.to_string()) {
        return Err(CommandError::Other(format!(
            "the recorded Slack message does not mention {rrid}, not approving; \
             re-run `request_review`"
        )));
    }

    let bot = slack.auth_test().await.ok();
    let acked: Vec<String> = message
        .reactions
        .iter()
        .filter(|r| is_ack_reaction(&r.name))
        .flat_map(|r| r.users.clone())
        // The bot acking its own request would approve nothing.
        .filter(|u| bot.as_deref() != Some(u.as_str()))
        .collect();

    if acked.is_empty() {
        return Err(CommandError::Other(format!(
            "the Slack review request for {rrid} has not been acknowledged, not approving; \
             ask a reviewer for a :+1: on the request"
        )));
    }

    session.display.println(&format!(
        "Slack review acknowledged by {}",
        acked.join(", ")
    ));
    Ok(())
}

/// Records the reviewer and commits the testreport to SVN (upstream
/// `_record_reviewer`). Returns `Err` when the record/commit fails so the
/// approval is aborted and the failure is surfaced (not swallowed).
async fn record_reviewer(session: &mut Session, name: &str) -> Result<(), CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::Other(
            "reviewer must be a non-empty string; not approving".to_owned(),
        ));
    }

    session.metadata_mut().set_reviewer(name).map_err(|e| {
        CommandError::Other(format!("failed to record reviewer, not approving: {e}"))
    })?;

    let checkout = session
        .metadata()
        .base()
        .report_wd()
        .map_err(|e| CommandError::Other(format!("no report loaded: {e}")))?;
    let install_logs = session.config.install_logs.clone();
    let msg = vec!["-m".to_owned(), format!("Add Test Plan Reviewer: {name}")];
    let runner = TokioSvnRunner;
    svn_commit_testreport(&runner, &checkout, &install_logs, &msg)
        .await
        .map_err(|e| {
            CommandError::Other(format!(
                "failed to commit testreport to SVN, not approving: {e}"
            ))
        })?;
    Ok(())
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

    /// Enable the Slack gate against a mock server, with a marker recorded.
    fn gated_session(
        server: &wiremock::MockServer,
        rrid: &str,
        marker: Option<(&str, &str)>,
    ) -> (Session, crate::commands::testkit::Buffer) {
        let (mut session, buf) = session_with_hosts(rrid, &["h1"], "ok");
        session.config.slack_enabled = true;
        session.config.slack_token = "xoxb-test".to_owned();
        session.config.slack_channel = "C1".to_owned();
        session.config.slack_api_url = server.uri();
        if let Some((channel, ts)) = marker {
            session.metadata_mut().base_mut().slack_review =
                Some(mtui_testreport::SlackReviewMarker {
                    channel: channel.to_owned(),
                    ts: ts.to_owned(),
                });
        }
        (session, buf)
    }

    async fn mount_message(
        server: &wiremock::MockServer,
        text: &str,
        reactions: serde_json::Value,
    ) {
        use wiremock::matchers::path;
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(path("/reactions.get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "message": { "text": text, "reactions": reactions }
            })))
            .mount(server)
            .await;
        Mock::given(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "user_id": "UBOT" })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn slack_gate_is_inert_when_the_integration_is_off() {
        // The default posture. Approve must behave exactly as it always has,
        // so every pre-existing approve test stays meaningful.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert!(!session.config.slack_enabled);
        let rrid = require_update(&session).unwrap();
        slack_review_gate(&mut session, &rrid).await.unwrap();
    }

    #[tokio::test]
    async fn slack_gate_refuses_when_no_review_was_requested() {
        let server = wiremock::MockServer::start().await;
        let (mut session, _buf) = gated_session(&server, "SUSE:Maintenance:1:1", None);
        let rrid = require_update(&session).unwrap();

        let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no review was requested"), "{msg}");
        assert!(msg.contains("request_review"), "says what to do: {msg}");
        // Nothing was asked of Slack: the marker check short-circuits.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn slack_gate_refuses_a_marker_pointing_at_another_update() {
        // The defence against a marker copied between templates: the message
        // must actually name this RRID, or it is not this update's review.
        let server = wiremock::MockServer::start().await;
        mount_message(
            &server,
            "Please review SUSE:Maintenance:9:9",
            serde_json::json!([{ "name": "+1", "users": ["U1"] }]),
        )
        .await;
        let (mut session, _buf) =
            gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
        let rrid = require_update(&session).unwrap();

        let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();
        assert!(err.to_string().contains("does not mention"), "{err}");
    }

    #[tokio::test]
    async fn slack_gate_refuses_an_unacknowledged_request() {
        let server = wiremock::MockServer::start().await;
        mount_message(
            &server,
            "Please review SUSE:Maintenance:1:1",
            serde_json::json!([{ "name": "eyes", "users": ["U1"] }]),
        )
        .await;
        let (mut session, _buf) =
            gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
        let rrid = require_update(&session).unwrap();

        let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();
        assert!(err.to_string().contains("not been acknowledged"), "{err}");
    }

    #[tokio::test]
    async fn slack_gate_ignores_the_bots_own_acknowledgement() {
        // A workspace that auto-reacts must not be able to self-approve.
        let server = wiremock::MockServer::start().await;
        mount_message(
            &server,
            "Please review SUSE:Maintenance:1:1",
            serde_json::json!([{ "name": "+1", "users": ["UBOT"] }]),
        )
        .await;
        let (mut session, _buf) =
            gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
        let rrid = require_update(&session).unwrap();

        let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();
        assert!(err.to_string().contains("not been acknowledged"), "{err}");
    }

    #[tokio::test]
    async fn slack_gate_passes_on_a_human_acknowledgement() {
        let server = wiremock::MockServer::start().await;
        mount_message(
            &server,
            "Please review SUSE:Maintenance:1:1 (recommended)",
            serde_json::json!([{ "name": "+1::skin-tone-2", "users": ["U1", "UBOT"] }]),
        )
        .await;
        let (mut session, buf) =
            gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
        let rrid = require_update(&session).unwrap();

        slack_review_gate(&mut session, &rrid).await.unwrap();
        // The human is named; the bot that shares the reaction is not.
        let out = buf.contents();
        assert!(out.contains("acknowledged by U1"), "{out}");
        assert!(!out.contains("UBOT"), "{out}");
    }

    #[tokio::test]
    async fn slack_gate_refuses_when_slack_is_unreachable() {
        // Fail closed: an unreadable review request is not an approved one.
        let server = wiremock::MockServer::start().await;
        use wiremock::matchers::path;
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(path("/reactions.get"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (mut session, _buf) =
            gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
        let rrid = require_update(&session).unwrap();

        let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();
        assert!(err.to_string().contains("not approving"), "{err}");
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn reviewer_with_no_template_path_errors() {
        // The report has no `path`, so set_reviewer fails → record_reviewer
        // returns Err → approve aborts with a surfaced error and never
        // dispatches (previously this was swallowed as Ok).
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Approve, &["-r", "alice"]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("record reviewer")));
        // Reviewer was NOT recorded (the write failed with no path).
        assert_eq!(session.metadata().base().reviewer, "");
    }

    #[tokio::test]
    async fn empty_reviewer_errors() {
        // A whitespace-only reviewer is rejected before any I/O.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Approve, &["-r", "  "]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("non-empty string")));
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // `#[serial(osc_config_env)]` guard makes the mutation exclusive.
    #[allow(unsafe_code)]
    async fn osc_dispatch_runs_for_maintenance_rrid() {
        // A Maintenance RRID routes to the native OBS backend. Point $OSC_CONFIG
        // at an oscrc that does not exist so credential resolution fails fast
        // (offline, no network), exercising the non-gitea dispatch + error
        // mapping without needing a real backend. (Group-approve is refused
        // before any I/O, but the missing-oscrc guard makes the failure
        // deterministic regardless.) `$OSC_CONFIG` is process-global → `#[serial]`.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config.session_user = "tester".to_owned();
        // SAFETY: serialised via `#[serial(osc_config_env)]`.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let args = matches(&Approve, &["-g", "qam-sle"]);
        let res = Approve.call(&mut session, &args).await;
        // SAFETY: still inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        // The native backend refuses group-approve / fails to resolve creds → Err;
        // the branch executed and the error mapping produced our message.
        if let Err(e) = res {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc approve failed")));
        }
    }

    #[tokio::test]
    async fn gitea_hash_match_proceeds_to_approve() {
        use mtui_datasources::assign_marker;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // SLFO report → Gitea path; the fake report's check_hash reports a match,
        // so the guard passes and gitea.approve runs. The comments GET reports
        // the acting user assigned to the group (and no decision yet), so the
        // approval posts its LGTM and succeeds, exercising the gitea success
        // branch + pi_autolock(Unlock) + the success confirmation.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/comments$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": 1,
                    "body": assign_marker("tester", "qam-sle"),
                    "updated_at": "2026-01-01T00:00:00Z"
                }])),
            )
            .mount(&server)
            .await;
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

        let (mut session, buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        session.config.session_user = "tester".to_owned();
        let args = matches(&Approve, &[]);
        Approve.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("approved SUSE:SLFO:1.2:5"),
            "expected success confirmation, got: {}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn gitea_approve_failure_is_surfaced() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // The comments GET returns 500 so gitea.approve fails; the failure must
        // be surfaced as a CommandError, not swallowed into an empty success.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, _buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("gitea approve failed")));
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
        session.activate("SUSE:SLFO:1.2:5");

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("hash differs")));
    }
}
