//! The `approve` command — approve the loaded update via OSC or Gitea.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{Gitea, GiteaError, Osc, Slack, is_ack_reaction};
use mtui_testreport::HashCheck;

use crate::command::{Command, Scope};
use crate::commands::apicall::{
    gitea_client, is_gitea_workflow, osc_client, require_committed_report,
};
use crate::commands::commit::{ReviewAction, prepare_reviewer};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The backend client, built before anything is uploaded.
enum Backend {
    /// The client, and the PR head the hash gate vouched for, if it knows one.
    Gitea(Box<Gitea>, Option<String>),
    Obs(Box<Osc>),
}

/// Approves the loaded update, dispatching to OSC or Gitea like the other
/// backend-API commands.
///
/// With `-r/--reviewer` the reviewer is stored on the report document and
/// uploaded to teregen with the artifacts *before* the approval; either failing
/// (or no document being loaded) aborts it. The reviewer and document checks,
/// the Slack gate, the backend client and the backend's own pre-checks (on
/// Gitea the hash gate and the assignment, on OBS the `-g` and assignment
/// checks) run before that upload, so an approval they refuse publishes
/// nothing; the OBS verdict check cannot, as it reads the uploaded document,
/// and on Gitea the LGTM is refused if the PR head moves during the upload.
/// Without `-r` nothing is uploaded: on Maintenance/OBS a report with
/// uncommitted edits is refused with a `commit` hint, because the verdict gate
/// reads the server's copy. On the Gitea path a checkout-hash mismatch prompts
/// for confirmation in the REPL (default no) and refuses non-interactively
/// unless `--accept-head SHA` names the PR head as it is now; a missing token
/// or a failed call always refuses.
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
        Scope::Explicit
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
                .help("Record reviewer on the report document, then approve"),
        )
        .arg(
            Arg::new("accept_head")
                .long("accept-head")
                .value_name("SHA")
                .help(
                    "Approve only if the PR head is exactly SHA; this is also what lets a head \
                     that moved since the report was generated through non-interactively \
                     (Gitea only)",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = [
            "-g",
            "--group",
            "-u",
            "--user",
            "-r",
            "--reviewer",
            "--accept-head",
        ]
        .iter()
        .filter(|f| f.starts_with(text))
        .map(|s| (*s).to_owned())
        .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let groups: Vec<String> = args
            .get_many::<String>("group")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        let user = args
            .get_one::<String>("user")
            .filter(|s| !s.is_empty())
            .cloned();

        // Everything that can refuse runs before the first upload, so a
        // refused approval leaves nothing published. Only the OBS verdict
        // check follows it: it reads the document the upload is about to store.
        let reviewer = args
            .get_one::<String>("reviewer")
            .map(|name| prepare_reviewer(session, name, ReviewAction::Approve))
            .transpose()?;
        if reviewer.is_none() {
            require_committed_report(session, &rrid, "approve")?;
        }
        slack_review_gate(session, &rrid).await?;
        let backend = if is_gitea_workflow(session) {
            let gitea = gitea_client(session)?;
            let head = hash_gate(
                session,
                args.get_one::<String>("accept_head").map(String::as_str),
            )
            .await?;
            gitea
                .check_decidable(user.as_deref())
                .await
                .map_err(|e| CommandError::Other(format!("gitea approve failed: {e}")))?;
            Backend::Gitea(Box::new(gitea), head)
        } else {
            let osc = osc_client(session, &rrid)?;
            osc.approve_preflight(&groups)
                .await
                .map_err(|e| CommandError::Other(format!("obs approve failed: {e}")))?;
            Backend::Obs(Box::new(osc))
        };

        if let Some(reviewer) = reviewer {
            // On Gitea the live hash verdict above replaces the stale flag
            // recorded at load time.
            let live_verdict = matches!(backend, Backend::Gitea(..));
            reviewer.record(session, live_verdict).await?;
            require_committed_report(session, &rrid, "approve")?;
        }

        match backend {
            Backend::Gitea(gitea, head) => {
                let approved = match head.as_deref() {
                    Some(head) => gitea.approve_if_head(user.as_deref(), head).await,
                    None => gitea.approve(user.as_deref()).await,
                };
                approved.map_err(|e| {
                    let hint = match e {
                        GiteaError::HeadMoved { .. } => {
                            "; review the new head, then re-run approve"
                        }
                        _ => "",
                    };
                    CommandError::Other(format!("gitea approve failed: {e}{hint}"))
                })?;
            }
            Backend::Obs(osc) => {
                tracing::info!("Approving request {}", rrid.review_id);
                osc.approve(&groups)
                    .await
                    .map_err(|e| CommandError::Other(format!("obs approve failed: {e}")))?;
            }
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
    if !message_mentions_rrid(&message.text, &rrid.to_string()) {
        return Err(CommandError::Other(format!(
            "the recorded Slack message does not mention {rrid}, not approving; \
             re-run `request_review`"
        )));
    }

    // Without the bot's ID its own reaction cannot be told from a human's.
    let bot = slack.auth_test().await.map_err(|e| {
        CommandError::Other(format!(
            "could not verify the Slack bot identity for {rrid}, not approving: {e}"
        ))
    })?;
    let acked: Vec<String> = message
        .reactions
        .iter()
        .filter(|r| is_ack_reaction(&r.name))
        .flat_map(|r| r.users.clone())
        // The bot acking its own request would approve nothing.
        .filter(|u| *u != bot)
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

/// Whether `text` names `rrid` as a whole whitespace-delimited token.
///
/// A substring test would take `SUSE:Maintenance:1:1` for a mention inside
/// `SUSE:Maintenance:1:10`. Only presentation punctuation is trimmed from a
/// token's edges; `:`, `_`, `-`, letters and digits never are, so a longer RRID
/// cannot reduce to this one.
fn message_mentions_rrid(text: &str, rrid: &str) -> bool {
    const EDGE: &[char] = &[
        '<', '>', '[', ']', '(', ')', '*', '`', '\'', '"', ',', ';', '.', '!', '?',
    ];
    text.split_whitespace()
        .any(|token| token.trim_matches(EDGE) == rrid)
}

/// Verifies the checked-out template's hash against the Gitea PR head before
/// approving, and returns the head the approval is vouched for (`None` when the
/// report records none to compare), so the LGTM can be refused if it moves.
///
/// `accept_head` is a pin: it must equal the PR head as it stands, whether or
/// not the report is stale, so a head that moved again is never waved through;
/// a different value refuses and names the current head. A
/// [`HashCheck::Mismatch`] is the only verdict an operator can otherwise act
/// on: in the REPL with a prompter installed it asks (default no), anywhere
/// else it refuses. [`HashCheck::MissingToken`] and [`HashCheck::Failed`]
/// produced no verdict at all, so both always refuse.
async fn hash_gate(
    session: &mut Session,
    accept_head: Option<&str>,
) -> Result<Option<String>, CommandError> {
    let accept_head = accept_head.map(str::trim);
    match session.metadata().check_hash().await {
        HashCheck::Ok => {
            // A match means the live head is the recorded hash.
            let head = session
                .metadata()
                .base()
                .giteacohash
                .clone()
                .filter(|h| !h.is_empty());
            match (accept_head, head.as_deref()) {
                (Some(given), Some(head)) if given != head => Err(CommandError::Other(format!(
                    "PR head is {head}, not {given}; not approving"
                ))),
                (Some(_), None) => Err(CommandError::Other(
                    "the report records no PR hash, so --accept-head cannot be checked; \
                     not approving"
                        .to_owned(),
                )),
                _ => Ok(head),
            }
        }
        HashCheck::Mismatch { expected, actual } => {
            tracing::error!(%expected, %actual, "GiteaPR hash differs from testreport");
            if let Some(given) = accept_head {
                if given == actual {
                    session.display.println(&format!(
                        "PR head {actual} accepted (testreport has {expected})"
                    ));
                    return Ok(Some(actual));
                }
                return Err(CommandError::Other(format!(
                    "PR head is {actual}, not {given}; not approving"
                )));
            }
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
                    return Ok(Some(actual));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Approve.name(), "approve");
        assert_eq!(Approve.scope(), Scope::Explicit);
    }

    #[test]
    fn completion_offers_reviewer_flag() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = Approve.complete(&session, "-r", "");
        assert_eq!(out, vec!["-r"]);
    }

    #[test]
    fn message_mentions_rrid_matches_whole_tokens_only() {
        const ID: &str = "SUSE:Maintenance:1:1";
        for text in [
            "Please review SUSE:Maintenance:1:1",
            "Please review SUSE:Maintenance:1:1 (recommended)",
            "Please review (SUSE:Maintenance:1:1)",
            "Please review *SUSE:Maintenance:1:1*",
            "Please review `SUSE:Maintenance:1:1`.",
            "Please review <SUSE:Maintenance:1:1>!",
            "SUSE:Maintenance:1:1",
            "Please review SUSE:Maintenance:9:9 and SUSE:Maintenance:1:1",
            "Please review SUSE:Maintenance:1:1\nnote: other SUSE:Maintenance:1:10",
            "Please\u{a0}review\u{2003}SUSE:Maintenance:1:1",
        ] {
            assert!(message_mentions_rrid(text, ID), "{text:?}");
        }
        for text in [
            "",
            "Please review SUSE:Maintenance:1:10",
            "Please review SUSE:Maintenance:1:1:2",
            "Please review SUSE:Maintenance:1:1_extra",
            "Please review SUSE:Maintenance:1:1-extra",
            "Please review SUSE:Maintenance:1:1.0",
            "Please review preSUSE:Maintenance:1:1",
            "Please review xSUSE:Maintenance:1:1",
            "Please review SUSE:Maintenance:9:9",
        ] {
            assert!(!message_mentions_rrid(text, ID), "{text:?}");
        }
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

    async fn mount_reactions(
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
    }

    async fn mount_auth(server: &wiremock::MockServer, response: wiremock::ResponseTemplate) {
        use wiremock::Mock;
        use wiremock::matchers::path;
        Mock::given(path("/auth.test"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn mount_message(
        server: &wiremock::MockServer,
        text: &str,
        reactions: serde_json::Value,
    ) {
        mount_reactions(server, text, reactions).await;
        mount_auth(
            server,
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "user_id": "UBOT" })),
        )
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
    async fn slack_gate_refuses_a_prefix_collision() {
        // `…:1:1` is a string prefix of `…:1:10`, a different request.
        let server = wiremock::MockServer::start().await;
        mount_message(
            &server,
            "Please review SUSE:Maintenance:1:10",
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

    /// Without a bot ID there is no telling the bot's reaction from a human's,
    /// so no acknowledgement can be trusted, whoever left it.
    #[tokio::test]
    async fn slack_gate_refuses_when_bot_identity_cannot_be_established() {
        use wiremock::ResponseTemplate;
        let bad_auth = [
            ("http 503", ResponseTemplate::new(503)),
            (
                "invalid_auth",
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": false, "error": "invalid_auth" })),
            ),
            (
                "missing user_id",
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            ),
            (
                "empty user_id",
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "user_id": "" })),
            ),
            (
                "blank user_id",
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "user_id": "  " })),
            ),
        ];
        for (case, auth) in bad_auth {
            for user in ["UBOT", "U1"] {
                let server = wiremock::MockServer::start().await;
                mount_reactions(
                    &server,
                    "Please review SUSE:Maintenance:1:1",
                    serde_json::json!([{ "name": "+1", "users": [user] }]),
                )
                .await;
                mount_auth(&server, auth.clone()).await;
                let (mut session, buf) =
                    gated_session(&server, "SUSE:Maintenance:1:1", Some(("C1", "1.0")));
                let rrid = require_update(&session).unwrap();

                let err = slack_review_gate(&mut session, &rrid).await.unwrap_err();

                let msg = err.to_string();
                assert!(msg.contains("bot identity"), "{case}/{user}: {msg}");
                assert!(msg.contains("not approving"), "{case}/{user}: {msg}");
                assert!(!msg.contains("xoxb"), "{case}/{user}: {msg}");
                assert!(!buf.contents().contains("acknowledged"), "{case}/{user}");
            }
        }
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
    async fn reviewer_without_a_document_aborts_the_approval() {
        // No document to record the reviewer on, so the approval must abort
        // with a surfaced error rather than dispatching.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Approve, &["-r", "alice"]);
        let err = Approve.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoDocument), "{err:?}");
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
            assert!(matches!(e, CommandError::Other(m) if m.contains("obs approve failed")));
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
                "requested_reviewers": [], "state": "open", "head": {"sha": "new"}
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

    /// `approve -r` must not publish anything when a later, side-effect-free
    /// check would refuse the approval.
    mod gates_first {
        use std::str::FromStr;

        use mtui_types::report_document::ReportDocument;
        use wiremock::matchers::{method, path as wpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::*;
        use crate::commands::testkit::NoOscrc;
        use crate::commands::testkit::teregen::{
            document_json, mount_auth_success, set_bare_report_wd, write_oscrc,
        };

        const ID: &str = "SUSE:SLFO:1.2:5";

        fn mismatch() -> HashCheck {
            HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "new".to_owned(),
            }
        }

        /// A teregen mock that accepts the auth handshake and the document PUT
        /// with a 2xx, and expects exactly `puts` of the latter. A 404 would
        /// fail the record step by itself and hide an upload.
        async fn teregen_expecting(puts: u64) -> MockServer {
            let server = MockServer::start().await;
            mount_auth_success(&server).await;
            Mock::given(method("PUT"))
                .and(wpath(format!("/reports/{ID}")))
                .respond_with(
                    ResponseTemplate::new(202)
                        .set_body_string(document_json(ID, "slfo"))
                        .insert_header("etag", "\"fresh\""),
                )
                .expect(puts)
                .mount(&server)
                .await;
            server
        }

        /// A Gitea-workflow session holding a report document, whose teregen
        /// writer is wired to `teregen`. `OSC_CONFIG` is left to the caller.
        fn gitea_session(
            verdict: HashCheck,
            gitea: &str,
            teregen: &MockServer,
        ) -> (Session, tempfile::TempDir) {
            let (mut session, _buf) = fixed_hash_session(verdict, gitea);
            session.config.gitea_url = gitea.to_owned();
            session.config.obs_api_url = teregen.uri();
            session.config.teregen_api_v2 = teregen.uri();
            let tmp = set_bare_report_wd(&mut session);
            let base = session.metadata_mut().base_mut();
            base.document = Some(ReportDocument::from_str(&document_json(ID, "slfo")).unwrap());
            base.document_etag = Some("\"stale\"".to_owned());
            (session, tmp)
        }

        /// Gitea mock for an approval: the acting user is assigned and no
        /// decision exists yet, so the LGTM POST goes through. The PR head is
        /// `head`.
        async fn gitea_approving(head: &str) -> MockServer {
            use mtui_datasources::assign_marker;
            use wiremock::matchers::path_regex;

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
                    "requested_reviewers": [], "state": "open", "head": {"sha": head}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;
            server
        }

        async fn approvals(gitea: &MockServer) -> usize {
            gitea
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method == wiremock::http::Method::POST)
                .count()
        }

        #[tokio::test]
        async fn accept_head_naming_the_current_head_approves_headless() {
            let gitea = gitea_approving("new").await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(mismatch(), &gitea.uri(), &teregen);
            assert!(!session.is_repl, "premise: no prompter to answer for us");

            let args = matches(&Approve, &["--accept-head", "new"]);
            Approve.call(&mut session, &args).await.unwrap();

            assert_eq!(approvals(&gitea).await, 1);
        }

        #[tokio::test]
        async fn accept_head_naming_another_head_refuses_and_names_the_current_one() {
            let gitea = gitea_approving("new").await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(mismatch(), &gitea.uri(), &teregen);

            let args = matches(&Approve, &["--accept-head", "older"]);
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m)
                    if m.contains("PR head is new, not older")),
                "{err:?}"
            );
            assert_eq!(approvals(&gitea).await, 0);
        }

        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn an_unassigned_gitea_pr_publishes_nothing() {
            let gitea = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
                .mount(&gitea)
                .await;
            let teregen = teregen_expecting(0).await;
            let dir = tempfile::tempdir().unwrap();
            let (mut session, _tmp) = gitea_session(HashCheck::Ok, &gitea.uri(), &teregen);

            let args = matches(&Approve, &["-r", "alice"]);
            let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.starts_with("gitea approve failed")),
                "{err:?}"
            );
            assert_eq!(session.metadata().base().reviewer, "");
            teregen.verify().await;
        }

        /// `-g` is argument validation, and an unassigned request is a read:
        /// neither needs the upload, so neither may follow it.
        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn the_obs_group_and_assignment_checks_publish_nothing() {
            use wiremock::matchers::path as any_path;

            let id = "SUSE:Maintenance:1:1";
            for (argv, wants) in [
                (vec!["-r", "alice", "-g", "qam-sle"], "group approval"),
                (vec!["-r", "alice"], "is not assigned"),
            ] {
                let teregen = teregen_expecting(0).await;
                Mock::given(method("GET"))
                    .and(any_path("/request/1"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        "<request id='1'><state name='review'/>\
                         <review state='new' by_group='qam-sle'/></request>",
                    ))
                    .mount(&teregen)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let (mut session, _buf) = session_with_hosts(id, &["h1"], "ok");
                session.config.obs_api_url = teregen.uri();
                session.config.teregen_api_v2 = teregen.uri();
                let _tmp = set_bare_report_wd(&mut session);
                let base = session.metadata_mut().base_mut();
                base.document =
                    Some(ReportDocument::from_str(&document_json(id, "maintenance")).unwrap());
                base.document_etag = Some("\"stale\"".to_owned());

                let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
                let err = Approve
                    .call(&mut session, &matches(&Approve, &argv))
                    .await
                    .unwrap_err();

                assert!(
                    matches!(&err, CommandError::Other(m)
                        if m.starts_with("obs approve failed") && m.contains(wants)),
                    "{wants}: {err:?}"
                );
                assert_eq!(session.metadata().base().reviewer, "", "{wants}");
                teregen.verify().await;
            }
        }

        /// Gate the session on a Slack review request whose message reads `text`
        /// and carries a `+1` from each of `users`. `/auth.test` is left to the
        /// caller.
        async fn enable_slack(
            session: &mut Session,
            slack: &MockServer,
            text: &str,
            users: &[&str],
        ) {
            mount_reactions(
                slack,
                text,
                serde_json::json!([{ "name": "+1", "users": users }]),
            )
            .await;
            session.config.slack_enabled = true;
            session.config.slack_token = "xoxb-test".to_owned();
            session.config.slack_channel = "C1".to_owned();
            session.config.slack_api_url = slack.uri();
            session.metadata_mut().base_mut().slack_review =
                Some(mtui_testreport::SlackReviewMarker {
                    channel: "C1".to_owned(),
                    ts: "1.0".to_owned(),
                });
        }

        fn bot_is(id: &str) -> ResponseTemplate {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": true, "user_id": id }))
        }

        /// The review request is for another request whose RRID merely starts
        /// with this one's: nothing may be recorded or approved. The matching
        /// message is the control that proves the same fixture can approve.
        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn a_slack_message_for_a_longer_rrid_publishes_and_approves_nothing() {
            for (text, approves) in [
                ("Please review SUSE:SLFO:1.2:50", false),
                ("Please review SUSE:SLFO:1.2:5", true),
            ] {
                let gitea = gitea_approving("new").await;
                let teregen = teregen_expecting(u64::from(approves)).await;
                let slack = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let (mut session, _tmp) = gitea_session(HashCheck::Ok, &gitea.uri(), &teregen);
                enable_slack(&mut session, &slack, text, &["U1"]).await;
                mount_auth(&slack, bot_is("UBOT")).await;
                let args = matches(&Approve, &["-r", "alice"]);

                let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
                let res = Approve.call(&mut session, &args).await;

                if approves {
                    res.unwrap();
                    assert_eq!(session.metadata().base().reviewer, "alice");
                    assert_eq!(approvals(&gitea).await, 1);
                } else {
                    let err = res.unwrap_err();
                    assert!(
                        matches!(&err, CommandError::Other(m) if m.contains("does not mention")),
                        "{err:?}"
                    );
                    assert_eq!(session.metadata().base().reviewer, "");
                    assert!(!session.metadata().base().document_dirty);
                    assert_eq!(approvals(&gitea).await, 0);
                    assert!(gitea.received_requests().await.unwrap().is_empty());
                }
                teregen.verify().await;
            }
        }

        /// A bot-only `+1` approves nothing once the bot's identity cannot be
        /// checked. The healthy `auth.test` with a human `+1` is the control
        /// that proves the same fixture can approve.
        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn an_unverifiable_slack_bot_identity_publishes_and_approves_nothing() {
            for (auth, users, approves) in [
                (ResponseTemplate::new(503), ["UBOT"], false),
                (bot_is(" "), ["UBOT"], false),
                (bot_is("UBOT"), ["U1"], true),
            ] {
                let gitea = gitea_approving("new").await;
                let teregen = teregen_expecting(u64::from(approves)).await;
                let slack = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let (mut session, _tmp) = gitea_session(HashCheck::Ok, &gitea.uri(), &teregen);
                enable_slack(
                    &mut session,
                    &slack,
                    "Please review SUSE:SLFO:1.2:5",
                    &users,
                )
                .await;
                mount_auth(&slack, auth).await;
                let args = matches(&Approve, &["-r", "alice"]);

                let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
                let res = Approve.call(&mut session, &args).await;

                if approves {
                    res.unwrap();
                    assert_eq!(session.metadata().base().reviewer, "alice");
                    assert_eq!(approvals(&gitea).await, 1);
                } else {
                    let err = res.unwrap_err();
                    assert!(
                        matches!(&err, CommandError::Other(m) if m.contains("bot identity")),
                        "{err:?}"
                    );
                    assert_eq!(session.metadata().base().reviewer, "");
                    assert!(!session.metadata().base().document_dirty);
                    assert!(gitea.received_requests().await.unwrap().is_empty());
                }
                teregen.verify().await;
            }
        }

        /// A bad reviewer name is refused before any Slack or Gitea round trip.
        #[tokio::test]
        async fn a_bad_reviewer_is_refused_before_any_round_trip() {
            let slack = MockServer::start().await;
            let (mut session, _buf) = gated_session(&slack, "SUSE:Maintenance:1:1", None);

            let err = Approve
                .call(&mut session, &matches(&Approve, &["-r", "  "]))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("non-empty string"), "{err}");
            let err = Approve
                .call(&mut session, &matches(&Approve, &["-r", "alice"]))
                .await
                .unwrap_err();
            assert!(matches!(err, CommandError::NoDocument), "{err:?}");

            assert!(slack.received_requests().await.unwrap().is_empty());
        }

        /// The head can move while the document uploads: the LGTM then goes
        /// unposted. Mutation caught: calling the unpinned `approve`.
        #[tokio::test]
        async fn a_head_that_moves_before_the_lgtm_is_not_approved() {
            let gitea = gitea_approving("moved").await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(mismatch(), &gitea.uri(), &teregen);

            let args = matches(&Approve, &["--accept-head", "new"]);
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m)
                    if m.contains("PR head moved from new to moved") && m.contains("re-run")),
                "{err:?}"
            );
            assert_eq!(approvals(&gitea).await, 0);
        }

        /// `--accept-head` pins the head even when the report is not stale.
        #[tokio::test]
        async fn accept_head_pins_the_head_of_a_matching_report_too() {
            let gitea = gitea_approving("abc123").await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(HashCheck::Ok, &gitea.uri(), &teregen);
            session.metadata_mut().base_mut().giteacohash = Some("abc123".to_owned());

            let other = matches(&Approve, &["--accept-head", "zzz"]);
            let err = Approve.call(&mut session, &other).await.unwrap_err();
            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("PR head is abc123, not zzz")),
                "{err:?}"
            );
            assert_eq!(approvals(&gitea).await, 0);

            let same = matches(&Approve, &["--accept-head", "abc123"]);
            Approve.call(&mut session, &same).await.unwrap();
            assert_eq!(approvals(&gitea).await, 1);
        }

        #[tokio::test]
        async fn accept_head_cannot_be_checked_against_a_report_without_a_hash() {
            let gitea = gitea_approving("abc").await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(HashCheck::Ok, &gitea.uri(), &teregen);
            assert_eq!(session.metadata().base().giteacohash, None);

            let args = matches(&Approve, &["--accept-head", "abc"]);
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(err.to_string().contains("records no PR hash"), "{err}");
            assert_eq!(approvals(&gitea).await, 0);
        }

        #[tokio::test]
        async fn accept_head_is_trimmed_but_never_a_prefix() {
            let server = MockServer::start().await;
            let teregen = teregen_expecting(0).await;
            let verdict = HashCheck::Mismatch {
                expected: "old".to_owned(),
                actual: "abcdef0123".to_owned(),
            };
            let (mut session, _tmp) = gitea_session(verdict, &server.uri(), &teregen);

            hash_gate(&mut session, Some(" abcdef0123\n"))
                .await
                .unwrap();
            let err = hash_gate(&mut session, Some("abcdef")).await.unwrap_err();

            assert!(err.to_string().contains("not abcdef;"), "{err}");
        }

        #[tokio::test]
        async fn accept_head_does_not_override_a_missing_verdict() {
            let server = MockServer::start().await;
            let teregen = teregen_expecting(0).await;
            let (mut session, _tmp) = gitea_session(
                HashCheck::Failed("boom".to_owned()),
                &server.uri(),
                &teregen,
            );

            let err = hash_gate(&mut session, Some("new")).await.unwrap_err();

            assert!(err.to_string().contains("boom"), "{err}");
        }

        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn accept_head_lets_a_stale_loaded_report_be_recorded_then_approved() {
            let gitea = gitea_approving("new").await;
            let teregen = teregen_expecting(1).await;
            let dir = tempfile::tempdir().unwrap();
            let (mut session, _tmp) = gitea_session(mismatch(), &gitea.uri(), &teregen);
            session.metadata_mut().base_mut().stale_hash_warning =
                Some("template hash mismatch (stale checkout)".to_owned());

            let args = matches(&Approve, &["-r", "alice", "--accept-head", "new"]);
            let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
            Approve.call(&mut session, &args).await.unwrap();

            assert_eq!(session.metadata().base().reviewer, "alice");
            teregen.verify().await;
            assert_eq!(approvals(&gitea).await, 1);
        }

        #[test]
        fn accept_head_completes() {
            let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
            assert_eq!(
                Approve.complete(&session, "--acc", ""),
                vec!["--accept-head"]
            );
        }

        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn a_refused_hash_gate_publishes_nothing() {
            let teregen = teregen_expecting(0).await;
            let dir = tempfile::tempdir().unwrap();
            let gitea = MockServer::start().await;
            let (mut session, _tmp) = gitea_session(mismatch(), &gitea.uri(), &teregen);
            let args = matches(&Approve, &["-r", "alice"]);

            let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("hash differs")),
                "{err:?}"
            );
            assert_eq!(session.metadata().base().reviewer, "");
            teregen.verify().await;
            assert!(gitea.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn a_missing_gitea_token_publishes_nothing() {
            let teregen = teregen_expecting(0).await;
            let dir = tempfile::tempdir().unwrap();
            let (mut session, _tmp) =
                gitea_session(HashCheck::Ok, "http://gitea.invalid/api", &teregen);
            session.config.gitea_token = String::new();
            let args = matches(&Approve, &["-r", "alice"]);

            let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("could not build Gitea client")),
                "{err:?}"
            );
            assert_eq!(session.metadata().base().reviewer, "");
            teregen.verify().await;
        }

        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        async fn an_unbuildable_http_client_publishes_nothing() {
            use mtui_config::SslVerify;

            let teregen = teregen_expecting(0).await;
            let dir = tempfile::tempdir().unwrap();
            let id = "SUSE:Maintenance:1:1";
            let (mut session, _buf) = session_with_hosts(id, &["h1"], "ok");
            session.config.obs_api_url = teregen.uri();
            session.config.teregen_api_v2 = teregen.uri();
            session.config.ssl_verify = SslVerify::CaBundle("/nonexistent/mtui-685-ca.pem".into());
            let _tmp = set_bare_report_wd(&mut session);
            let base = session.metadata_mut().base_mut();
            base.document =
                Some(ReportDocument::from_str(&document_json(id, "maintenance")).unwrap());
            base.document_etag = Some("\"stale\"".to_owned());
            let args = matches(&Approve, &["-r", "alice"]);

            let _oscrc = NoOscrc::set_to(&write_oscrc(dir.path(), &teregen));
            let err = Approve.call(&mut session, &args).await.unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.starts_with("could not build ")),
                "{err:?}"
            );
            assert_eq!(session.metadata().base().reviewer, "");
            teregen.verify().await;
        }
    }

    mod document_path {
        use wiremock::matchers::{method, path as wpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use mtui_types::report_document::Req;

        use super::*;
        use crate::commands::commit::record_reviewer_document;
        use crate::commands::testkit::teregen::{
            document_put_body, minimal_document, mount_auth_success, set_bare_report_wd,
            store_path, teregen_v2_client,
        };

        const ID: &str = "SUSE:Maintenance:1:1";

        fn stored_document(reviewer: &str) -> String {
            let mut doc = minimal_document(ID);
            doc.people.reviewer.name = Req(Some(reviewer.to_owned()));
            serde_json::to_string(&doc).unwrap()
        }

        /// A document-loaded session whose report dir holds `install_logs/h1.log`.
        fn session_with_artifact() -> (Session, tempfile::TempDir) {
            let (mut session, _buf) = session_with_hosts(ID, &["h1"], "ok");
            let tmp = set_bare_report_wd(&mut session);
            std::fs::create_dir_all(tmp.path().join("install_logs")).unwrap();
            std::fs::write(tmp.path().join("install_logs/h1.log"), b"log").unwrap();
            let base = session.metadata_mut().base_mut();
            base.document = Some(minimal_document(ID));
            base.document_etag = Some("\"stale\"".to_owned());
            (session, tmp)
        }

        async fn mount_artifact(server: &MockServer, status: u16, expected: u64) {
            Mock::given(method("PUT"))
                .and(wpath(format!("/reports/{ID}/artifacts/h1.log")))
                .respond_with(ResponseTemplate::new(status))
                .expect(expected)
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn records_the_reviewer_on_the_document_and_uploads_the_artifacts() {
            let server = MockServer::start().await;
            let (_store, store_file) = store_path();
            mount_auth_success(&server).await;
            Mock::given(method("PUT"))
                .and(wpath(format!("/reports/{ID}")))
                .respond_with(
                    ResponseTemplate::new(202)
                        .set_body_string(stored_document("alice"))
                        .insert_header("etag", "\"fresh\""),
                )
                .mount(&server)
                .await;
            mount_artifact(&server, 201, 1).await;
            let (mut session, tmp) = session_with_artifact();
            let client = teregen_v2_client(&server, store_file);

            record_reviewer_document(&mut session, &client, "alice", ReviewAction::Approve, false)
                .await
                .unwrap();

            let body = document_put_body(&server, ID).await;
            assert_eq!(body.pointer("/people/reviewer/name"), Some(&"alice".into()));
            let base = session.metadata().base();
            assert_eq!(base.reviewer, "alice");
            assert!(!base.document_dirty);
            assert!(!tmp.path().join("log").exists());
        }

        #[tokio::test]
        async fn a_412_refuses_and_leaves_everything_as_it_was() {
            let server = MockServer::start().await;
            let (_store, store_file) = store_path();
            mount_auth_success(&server).await;
            Mock::given(method("PUT"))
                .and(wpath(format!("/reports/{ID}")))
                .respond_with(ResponseTemplate::new(412).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;
            mount_artifact(&server, 201, 0).await;
            let (mut session, _tmp) = session_with_artifact();
            let before = session.metadata().base().document.clone();
            let client = teregen_v2_client(&server, store_file);

            let err = record_reviewer_document(
                &mut session,
                &client,
                "alice",
                ReviewAction::Approve,
                false,
            )
            .await
            .unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("not approving")),
                "{err:?}"
            );
            let base = session.metadata().base();
            assert_eq!(base.reviewer, "");
            assert_eq!(base.document, before);
            assert!(!base.document_dirty);
        }

        #[tokio::test]
        async fn a_failed_artifact_refuses_but_the_document_is_already_stored() {
            let server = MockServer::start().await;
            let (_store, store_file) = store_path();
            mount_auth_success(&server).await;
            Mock::given(method("PUT"))
                .and(wpath(format!("/reports/{ID}")))
                .respond_with(
                    ResponseTemplate::new(202)
                        .set_body_string(stored_document("alice"))
                        .insert_header("etag", "\"fresh\""),
                )
                .mount(&server)
                .await;
            mount_artifact(&server, 500, 1).await;
            let (mut session, _tmp) = session_with_artifact();
            let client = teregen_v2_client(&server, store_file);

            let err = record_reviewer_document(
                &mut session,
                &client,
                "alice",
                ReviewAction::Approve,
                false,
            )
            .await
            .unwrap_err();

            assert!(
                matches!(&err, CommandError::Other(m)
                    if m.contains("1 artifact") && m.contains("re-run approve")),
                "{err:?}"
            );
            let base = session.metadata().base();
            assert_eq!(base.reviewer, "");
            assert_eq!(base.document_etag.as_deref(), Some("\"fresh\""));
        }

        /// `approve --reviewer` on a document report reaches the teregen writer
        /// (and so fails on the missing oscrc) before any OBS call.
        #[tokio::test]
        #[serial_test::serial(osc_config_env)]
        // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes
        // the mutation of the process-global `$OSC_CONFIG` exclusive.
        #[allow(unsafe_code)]
        async fn the_call_routes_a_document_report_to_teregen() {
            let (mut session, _buf) = session_with_hosts(ID, &["h1"], "ok");
            session.metadata_mut().base_mut().document = Some(minimal_document(ID));
            let args = matches(&Approve, &["-r", "alice"]);

            // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
            unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
            let res = Approve.call(&mut session, &args).await;
            // SAFETY: still inside that critical section.
            unsafe { std::env::remove_var("OSC_CONFIG") };

            let err = res.unwrap_err();
            assert!(
                matches!(&err, CommandError::Other(m) if m.contains("could not read oscrc credentials")),
                "{err:?}"
            );
        }
    }
}
