//! The `approve` command — approve the loaded update via OSC or Gitea.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{Osc, Slack, is_ack_reaction};
use mtui_testreport::{HashCheck, TokioSvnRunner, svn_commit_testreport};

use crate::command::{Command, Scope};
use crate::commands::apicall::{gitea_client, is_gitea_workflow};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Approves the loaded update, dispatching to OSC or Gitea like the other
/// backend-API commands.
///
/// With `-r/--reviewer` the reviewer is recorded and the template committed to
/// SVN *before* the approval; either failing aborts it. On the Gitea path a
/// checkout-hash mismatch prompts for confirmation in the REPL (default no) and
/// refuses non-interactively; a missing token or a failed call always refuses.
/// Unlocks PI reference hosts afterwards.
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

        // Before any state change, so a refused approval leaves nothing
        // half-done.
        slack_review_gate(session, &rrid).await?;

        // Record + commit before approving; abort on failure.
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

        if is_gitea_workflow(session) {
            let gitea = gitea_client(session)?;
            hash_gate(session).await?;
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

        session.display.println(&format!("approved {rrid}"));
        Ok(())
    }
}

/// Refuse the approval unless the update's Slack review request was acked.
///
/// A no-op unless `[slack] enabled = true`. Once on there is deliberately no
/// per-invocation bypass — a gate with one is not a gate; turning it off is an
/// explicit, auditable `config set slack_enabled false`.
///
/// Three things must hold, each ruling out a way of approving something nobody
/// reviewed: a marker exists (else no review was requested); the marked message
/// still names this RRID (else a marker copied from another template would
/// launder an approval for an unexamined update); and someone other than the bot
/// left an approving reaction. Only `approve` is gated — blocking `reject` would
/// strand an update a reviewer wants stopped.
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

    // A message that does not name this RRID is not this update's review,
    // whatever the template claims.
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

/// Verifies the checked-out template's hash against the Gitea PR head before
/// approving. A [`HashCheck::Mismatch`] is the only verdict an operator can act
/// on: in the REPL, with a prompter installed, it asks (default no); anywhere
/// else it refuses. [`HashCheck::MissingToken`] and [`HashCheck::Failed`]
/// produced no verdict at all, so both always refuse.
async fn hash_gate(session: &mut Session) -> Result<(), CommandError> {
    match session.metadata().check_hash().await {
        HashCheck::Ok => Ok(()),
        HashCheck::Mismatch { expected, actual } => {
            tracing::error!(%expected, %actual, "GiteaPR hash differs from testreport");
            if session.is_repl
                && let Some(p) = session.prompter()
            {
                let confirmed = p
                    .confirm(
                        &format!(
                            "GiteaPR hash differs from testreport ({expected} -> {actual}); \
                             approve anyway? [y/N]: "
                        ),
                        false,
                    )
                    .await;
                if confirmed {
                    return Ok(());
                }
                return Err(CommandError::Other(format!(
                    "GiteaPR hash differs from testreport ({expected} -> {actual}); \
                     not approving"
                )));
            }
            Err(CommandError::Other(format!(
                "GiteaPR hash differs from testreport ({expected} -> {actual}); \
                 refusing to approve non-interactively"
            )))
        }
        HashCheck::MissingToken => Err(CommandError::Other(
            "no Gitea token configured; cannot verify the PR hash, not approving".to_owned(),
        )),
        HashCheck::Failed(e) => Err(CommandError::Other(format!(
            "Gitea call failed: {e}; cannot verify the PR hash, not approving"
        ))),
    }
}

/// Records the reviewer and commits the testreport to SVN. `Err` aborts the
/// approval rather than swallowing the record/commit failure.
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
        // The default posture, under which every other approve test runs.
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
        // A marker copied between templates: the message must name this RRID.
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
        // The human is named; the bot sharing the reaction is not.
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
        // No `path` on the report, so `set_reviewer` fails and the approval must
        // abort with a surfaced error rather than dispatching.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Approve, &["-r", "alice"]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("record reviewer")));
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
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes the
    // mutation of the process-global `$OSC_CONFIG` exclusive.
    #[allow(unsafe_code)]
    async fn osc_dispatch_runs_for_maintenance_rrid() {
        // A missing oscrc makes credential resolution fail fast offline, so the
        // non-gitea dispatch and its error mapping run without a real backend.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config.session_user = "tester".to_owned();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let args = matches(&Approve, &["-g", "qam-sle"]);
        let res = Approve.call(&mut session, &args).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        if let Err(e) = res {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc approve failed")));
        }
    }

    #[tokio::test]
    async fn gitea_hash_match_proceeds_to_approve() {
        use mtui_datasources::assign_marker;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // check_hash matches, and the comments GET has the acting user assigned
        // with no decision yet, so the LGTM posts and the Gitea path succeeds.
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
        session.metadata_mut().base_mut().update_source = mtui_types::UpdateSource::Git;
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

        // A 500 must surface as a CommandError, not an empty success.
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
        session.metadata_mut().base_mut().update_source = mtui_types::UpdateSource::Git;
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("gitea approve failed")));
    }

    // Drives the hash-gate branches without a real Gitea PR head.
    use async_trait::async_trait;
    use mtui_testreport::{TestReport, TestReportBase};
    use std::collections::HashMap;

    struct FixedHashReport {
        base: TestReportBase,
        verdict: HashCheck,
    }
    #[async_trait]
    impl TestReport for FixedHashReport {
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
            self.verdict.clone()
        }
    }

    /// A session with an active Gitea-workflow report whose `check_hash`
    /// returns `verdict`, pointed at `giteaprapi`.
    fn fixed_hash_session(
        verdict: HashCheck,
        giteaprapi: &str,
    ) -> (Session, crate::commands::testkit::Buffer) {
        let (mut session, buf) = empty_session();
        let mut base = TestReportBase::new(mtui_config::Config::default());
        base.rrid = "SUSE:SLFO:1.2:5".parse().ok();
        base.update_source = mtui_types::UpdateSource::Git;
        base.giteaprapi = Some(giteaprapi.to_owned());
        session.config.gitea_token = "tok".to_owned();
        session.config.session_user = "tester".to_owned();
        session
            .templates
            .add(Box::new(FixedHashReport { base, verdict }));
        assert!(
            session.activate("SUSE:SLFO:1.2:5").is_active(),
            "seeded template must activate"
        );
        (session, buf)
    }

    /// A prompter that always answers `answer`, ignoring the prompt text.
    fn fixed_prompter(answer: &'static str) -> mtui_hosts::Prompter {
        mtui_hosts::Prompter::new(std::sync::Arc::new(move |_t: String| {
            Box::pin(async move { Ok(answer.to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }))
    }

    #[tokio::test]
    async fn gitea_hash_mismatch_aborts_headless() {
        // is_repl is false (the default), so the guard refuses without ever
        // consulting a prompter.
        let (mut session, _buf) = fixed_hash_session(
            HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "new".to_owned(),
            },
            "http://gitea.invalid/api",
        );

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("hash differs")));
    }

    #[tokio::test]
    async fn gitea_hash_mismatch_declined_in_repl_never_approves() {
        // The mock has no mounts, so an empty request log is positive proof
        // gitea.approve was never reached.
        let server = wiremock::MockServer::start().await;
        let (mut session, _buf) = fixed_hash_session(
            HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "new".to_owned(),
            },
            &server.uri(),
        );
        session.config.gitea_url = server.uri();
        session.is_repl = true;
        session.set_prompter(fixed_prompter("n"));

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("not approving")));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gitea_hash_mismatch_confirmed_in_repl_approves() {
        use mtui_datasources::assign_marker;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

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

        let (mut session, buf) = fixed_hash_session(
            HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "new".to_owned(),
            },
            &server.uri(),
        );
        session.config.gitea_url = server.uri();
        session.is_repl = true;
        session.set_prompter(fixed_prompter("y"));

        let args = matches(&Approve, &[]);
        Approve.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("approved SUSE:SLFO:1.2:5"),
            "expected success confirmation, got: {}",
            buf.contents()
        );
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .any(|r| r.method == wiremock::http::Method::POST),
            "expected a POST, got: {requests:?}"
        );
    }

    #[tokio::test]
    async fn gitea_hash_mismatch_in_repl_without_prompter_refuses() {
        // is_repl with no prompter (the `mtui-mcp` posture) must still refuse,
        // without ever calling Gitea.
        let server = wiremock::MockServer::start().await;
        let (mut session, _buf) = fixed_hash_session(
            HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "new".to_owned(),
            },
            &server.uri(),
        );
        session.config.gitea_url = server.uri();
        session.is_repl = true;

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(err, CommandError::Other(m) if m.contains("refusing to approve non-interactively"))
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gitea_hash_check_failure_refuses_in_repl() {
        // `Failed` produced no verdict, so there is nothing to confirm: it must
        // refuse without consulting this prompter, which would answer yes.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let prompter = mtui_hosts::Prompter::new(std::sync::Arc::new(move |_t: String| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok("y".to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }));

        let (mut session, _buf) = fixed_hash_session(
            HashCheck::Failed("boom".to_owned()),
            "http://gitea.invalid/api",
        );
        session.is_repl = true;
        session.set_prompter(prompter);

        let args = matches(&Approve, &[]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("boom")));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
