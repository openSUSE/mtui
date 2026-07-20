//! The `request_review` command — ask for review of the loaded update in Slack.
//!
//! Posts a review request naming the update's RRID into the configured channel,
//! records the resulting message in the testreport template so the request is
//! traceable afterwards, and optionally watches the message for reviewer
//! reactions.
//!
//! Three design decisions are worth stating, because each departs from the
//! obvious reading:
//!
//! * **The watch is opt-in (`--watch`), not the default.** A watch runs for up
//!   to an hour, and the same command has to behave sanely over MCP, where a
//!   blocking call that outlives the client's timeout is indistinguishable from
//!   a hang. Posting is fast and total; watching is the long-running extra the
//!   caller asks for, and over MCP it belongs in a background job.
//! * **The marker is written but not committed.** Persisting it to SVN would
//!   couple posting a chat message to a working checkout and a network commit.
//!   mtui already has an explicit `commit`, and the marker rides along with it
//!   like every other template edit.
//! * **Rate limiting is not failure.** Slack throttles routinely; a `429`
//!   leaves the watch running rather than counting against it, or a busy
//!   channel would end the watch early and report "no reaction" when the truth
//!   is "we were not allowed to look".

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{PostedMessage, Slack, SlackError, is_ack_reaction, is_nack_reaction};
use mtui_testreport::SlackReviewMarker;

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// How long to wait after a rate-limit reply that carried no `Retry-After`.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(30);

/// Upper bound on any single back-off, so a hostile or mistaken `Retry-After`
/// cannot park the watch for hours.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Consecutive hard failures tolerated before the watch gives up.
///
/// Transient errors happen; a persistent one (the message was deleted, the
/// token was revoked) should end the watch rather than spin until timeout.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// How a watch ended, so the summary can say something true.
#[derive(Debug, PartialEq, Eq)]
enum WatchOutcome {
    /// A reviewer reacted with an approval emoji.
    Approved(Vec<String>),
    /// A reviewer reacted with a rejection emoji.
    Rejected(Vec<String>),
    /// The watch window elapsed with no verdict.
    TimedOut,
    /// The user pressed Ctrl-C.
    Interrupted,
    /// The watch stopped because Slack kept failing.
    Failed(String),
}

/// Spread a poll interval by ±15% so several mtui instances watching the same
/// channel do not converge on the same request times.
///
/// Uses the clock's sub-second noise rather than pulling in a random-number
/// dependency: the goal is decorrelation between processes, not unpredictability.
fn jittered(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    // Map the noise onto [0.85, 1.15] in integer arithmetic.
    let factor = 85 + (nanos % 31);
    Duration::from_millis(base.as_millis() as u64 * factor / 100)
}

/// The message posted to Slack.
///
/// The RRID appears verbatim so the request can be traced back to the update
/// from Slack alone, and so any later verification can bind to this exact
/// message rather than to "some review request".
fn review_message(rrid: &str, category: &str, note: Option<&str>) -> String {
    let mut msg = format!("Please review {rrid}");
    if !category.is_empty() {
        msg.push_str(&format!(" ({category})"));
    }
    if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
        msg.push('\n');
        msg.push_str(note.trim());
    }
    msg
}

/// Build a Slack client from the session config, mapping the refusal reasons
/// to command errors that say what to do about them.
fn slack_client(session: &Session) -> Result<Slack, CommandError> {
    Slack::new(&session.config).map_err(|e| match e {
        // The common case for anyone not using the integration: say so plainly
        // rather than making them read it as a failure.
        SlackError::Disabled => CommandError::Other(
            "Slack integration is disabled; enable it with `config set slack_enabled true` \
             or the `[slack] enabled` config key"
                .to_owned(),
        ),
        other => CommandError::Other(format!("could not build Slack client: {other}")),
    })
}

/// Resolve the channel to post to: `--channel` wins over the configured one.
fn resolve_channel(session: &Session, args: &ArgMatches) -> Result<String, CommandError> {
    let channel = args
        .get_one::<String>("channel")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| session.config.slack_channel.clone());
    if channel.is_empty() {
        return Err(CommandError::Other(format!(
            "{}",
            SlackError::MissingChannel
        )));
    }
    Ok(channel)
}

/// Poll `posted` until a verdict, the deadline, or Ctrl-C.
///
/// Returns the outcome rather than printing it, so the caller owns all display
/// and the loop stays testable.
async fn watch(
    slack: &Slack,
    posted: &PostedMessage,
    poll: Duration,
    timeout: Duration,
    bot_id: Option<&str>,
) -> WatchOutcome {
    let deadline = Instant::now() + timeout;
    let mut failures: u32 = 0;

    loop {
        // Poll before sleeping, so an already-acked request is recognised
        // immediately and an elapsed deadline still gets exactly one look.
        match slack.reactions(&posted.channel, &posted.ts).await {
            Ok(reactions) => {
                failures = 0;
                let voters = |names: &[String]| -> Vec<String> {
                    let mut v: Vec<String> = names.to_vec();
                    // The bot's own reactions are not review decisions.
                    if let Some(bot) = bot_id {
                        v.retain(|u| u != bot);
                    }
                    v.sort();
                    v.dedup();
                    v
                };
                let acked: Vec<String> = voters(
                    &reactions
                        .iter()
                        .filter(|r| is_ack_reaction(&r.name))
                        .flat_map(|r| r.users.clone())
                        .collect::<Vec<_>>(),
                );
                let nacked: Vec<String> = voters(
                    &reactions
                        .iter()
                        .filter(|r| is_nack_reaction(&r.name))
                        .flat_map(|r| r.users.clone())
                        .collect::<Vec<_>>(),
                );
                // A rejection is checked first: if a reviewer left both, the
                // objection is the one that must not be silently outvoted.
                if !nacked.is_empty() {
                    return WatchOutcome::Rejected(nacked);
                }
                if !acked.is_empty() {
                    return WatchOutcome::Approved(acked);
                }
            }
            // Throttling is not a failure — it is Slack asking us to wait.
            Err(SlackError::RateLimited { retry_after }) => {
                let wait = retry_after
                    .map_or(DEFAULT_BACKOFF, Duration::from_secs)
                    .min(MAX_BACKOFF);
                tracing::debug!(?wait, "rate limited; backing off");
                if sleep_or_interrupt(jittered(wait), deadline).await {
                    return WatchOutcome::Interrupted;
                }
                if Instant::now() >= deadline {
                    return WatchOutcome::TimedOut;
                }
                continue;
            }
            Err(e) => {
                failures += 1;
                let msg = e.to_string();
                tracing::warn!(failures, "Slack reaction poll failed: {msg}");
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    return WatchOutcome::Failed(msg);
                }
            }
        }

        if Instant::now() >= deadline {
            return WatchOutcome::TimedOut;
        }
        if sleep_or_interrupt(jittered(poll), deadline).await {
            return WatchOutcome::Interrupted;
        }
    }
}

/// Sleep for `dur` (never past `deadline`), returning `true` if Ctrl-C arrived.
///
/// Nothing in the CLI installs a SIGINT handler today, so without this a
/// Ctrl-C during a watch kills the process outright and the user never learns
/// that their review request was in fact posted.
async fn sleep_or_interrupt(dur: Duration, deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let dur = dur.min(remaining);
    tokio::select! {
        () = tokio::time::sleep(dur) => false,
        _ = tokio::signal::ctrl_c() => true,
    }
}

/// Requests review of the loaded update in Slack.
pub struct RequestReview;

#[async_trait]
impl Command for RequestReview {
    fn name(&self) -> &'static str {
        "request_review"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Requests review of the loaded update in Slack.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("channel")
                .short('c')
                .long("channel")
                .value_name("CHANNEL")
                .help("Channel to post to, overriding the configured one"),
        )
        .arg(
            Arg::new("message")
                .short('m')
                .long("message")
                .value_name("TEXT")
                .help("Extra context appended to the review request"),
        )
        .arg(
            Arg::new("watch")
                .short('w')
                .long("watch")
                .action(ArgAction::SetTrue)
                .help(
                    "After posting, watch the message for reviewer reactions until a \
                     verdict or timeout (Ctrl-C stops it). Over MCP, pair this with \
                     background=true so the call does not outlive the client timeout",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = ["-c", "--channel", "-m", "--message", "-w", "--watch"]
            .iter()
            .filter(|f| f.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let slack = slack_client(session)?;
        let channel = resolve_channel(session, args)?;

        // Validate the token before posting, so a bad one is reported as such
        // rather than as a confusing failure on the post itself.
        let bot_id = match slack.auth_test().await {
            Ok(id) => Some(id),
            Err(e) => {
                return Err(CommandError::Other(format!(
                    "Slack authentication failed: {e}"
                )));
            }
        };

        let category = session.metadata().base().category.clone();
        let note = args.get_one::<String>("message").map(String::as_str);
        let text = review_message(&rrid.to_string(), &category, note);

        let posted = slack
            .post_message(&channel, &text)
            .await
            .map_err(|e| CommandError::Other(format!("failed to post review request: {e}")))?;

        // Record the message before anything else can fail: from here on the
        // request exists in Slack, and the template should say so even if the
        // watch below is interrupted.
        let marker = SlackReviewMarker {
            channel: posted.channel.clone(),
            ts: posted.ts.clone(),
        };
        let recorded = match session.metadata_mut().set_slack_review(&marker) {
            Ok(()) => true,
            Err(e) => {
                // The request is already posted; failing the whole command here
                // would misreport a real, visible message as not sent.
                let msg = session.display.yellow(&format!(
                    "warning: review request posted, but recording it in the template failed: {e}"
                ));
                session.display.println(&msg);
                false
            }
        };

        session
            .display
            .println(&format!("requested review of {rrid} in {channel}"));
        if recorded {
            session
                .display
                .println("recorded the request in the template (commit to share it)");
        }

        if !args.get_flag("watch") {
            return Ok(());
        }

        let poll = Duration::from_secs(session.config.slack_poll_interval);
        let timeout = Duration::from_secs(session.config.slack_watch_timeout);
        session.display.println(&format!(
            "watching for reactions (up to {}s, Ctrl-C to stop)",
            timeout.as_secs()
        ));

        let outcome = watch(&slack, &posted, poll, timeout, bot_id.as_deref()).await;
        report(session, &rrid.to_string(), &outcome);

        // A failed watch is a failed command: over MCP the caller needs a
        // failed tool call, not a success whose text happens to say otherwise.
        if let WatchOutcome::Failed(e) = outcome {
            return Err(CommandError::Other(format!("Slack watch failed: {e}")));
        }
        Ok(())
    }
}

/// Print the watch result.
fn report(session: &mut Session, rrid: &str, outcome: &WatchOutcome) {
    let line = match outcome {
        WatchOutcome::Approved(users) => session.display.green(&format!(
            "{rrid}: review approved in Slack by {}",
            users.join(", ")
        )),
        WatchOutcome::Rejected(users) => session.display.red(&format!(
            "{rrid}: review rejected in Slack by {}",
            users.join(", ")
        )),
        WatchOutcome::TimedOut => session
            .display
            .yellow(&format!("{rrid}: no review reaction before the timeout")),
        WatchOutcome::Interrupted => session.display.yellow(&format!(
            "{rrid}: stopped watching; the request is still posted"
        )),
        WatchOutcome::Failed(e) => session.display.red(&format!(
            "{rrid}: stopped watching after repeated errors: {e}"
        )),
    };
    session.display.println(&line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_with_hosts};
    use mtui_datasources::{HttpClient, VerifyPolicy};
    use serde_json::json;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CHANNEL: &str = "C0123456789";
    const TS: &str = "1700000000.000100";

    fn slack_for(server: &MockServer) -> Slack {
        let http = HttpClient::new(VerifyPolicy::Default(true)).expect("client builds");
        Slack::with_client(http, "xoxb-test".to_owned(), &server.uri()).expect("builds")
    }

    async fn mount(server: &MockServer, api_method: &str, body: serde_json::Value) {
        Mock::given(path(format!("/{api_method}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Mount the auth + post endpoints a successful `call` needs.
    async fn mount_post_path(server: &MockServer) {
        mount(
            server,
            "auth.test",
            json!({ "ok": true, "user_id": "UBOT" }),
        )
        .await;
        mount(
            server,
            "chat.postMessage",
            json!({ "ok": true, "channel": CHANNEL, "ts": TS }),
        )
        .await;
    }

    /// A session with Slack enabled and pointed at `server`.
    fn slack_session(server: &MockServer) -> (Session, crate::commands::testkit::Buffer) {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:2", &["h1"], "ok");
        session.config.slack_enabled = true;
        session.config.slack_token = "xoxb-test".to_owned();
        session.config.slack_channel = CHANNEL.to_owned();
        session.config.slack_api_url = server.uri();
        (session, buf)
    }

    #[test]
    fn review_message_names_the_rrid_verbatim() {
        // The RRID must survive into the message text: it is what ties a Slack
        // thread back to an update.
        let msg = review_message("SUSE:Maintenance:1:2", "recommended", None);
        assert!(msg.contains("SUSE:Maintenance:1:2"), "{msg}");
        assert!(msg.contains("recommended"), "{msg}");

        // An extra note is appended, and an empty one adds no stray blank line.
        let with_note = review_message("SUSE:Maintenance:1:2", "", Some("  urgent  "));
        assert!(with_note.ends_with("urgent"), "{with_note}");
        assert_eq!(review_message("R", "", Some("   ")), "Please review R");
    }

    #[test]
    fn jitter_stays_within_fifteen_percent() {
        let base = Duration::from_secs(100);
        for _ in 0..50 {
            let j = jittered(base);
            assert!(
                j >= Duration::from_secs(85) && j <= Duration::from_secs(115),
                "jitter out of range: {j:?}"
            );
        }
    }

    #[tokio::test]
    async fn disabled_slack_refuses_with_an_actionable_message() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:2", &["h1"], "ok");
        // The default state: the integration is off.
        let args = matches(&RequestReview, &[]);
        let err = RequestReview.call(&mut session, &args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("disabled"), "{msg}");
        assert!(msg.contains("slack_enabled"), "says how to fix it: {msg}");
    }

    #[tokio::test]
    async fn missing_channel_refuses_before_posting() {
        let server = MockServer::start().await;
        let (mut session, _buf) = slack_session(&server);
        session.config.slack_channel = String::new();

        let args = matches(&RequestReview, &[]);
        let err = RequestReview.call(&mut session, &args).await.unwrap_err();
        assert!(err.to_string().contains("channel"), "{err}");
        // Nothing was sent: the guard runs before any request.
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn posts_and_records_the_marker() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        let (mut session, buf) = slack_session(&server);

        // Give the report a template file so the marker can be written.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "Test Plan Reviewer: bob\n").unwrap();
        session.metadata_mut().base_mut().path = Some(path.clone());

        let args = matches(&RequestReview, &[]);
        RequestReview.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents().contains("requested review"),
            "{}",
            buf.contents()
        );
        // The canonical channel from the post response is what gets recorded,
        // not the configured name.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains(&format!("Slack Review: {CHANNEL} {TS}")),
            "{written}"
        );
        assert_eq!(
            session.metadata().base().slack_review.as_ref().unwrap().ts,
            TS
        );
    }

    #[tokio::test]
    async fn a_failed_marker_write_warns_but_keeps_the_post_reported() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        let (mut session, buf) = slack_session(&server);
        // No template path: the marker write fails.
        session.metadata_mut().base_mut().path = None;

        let args = matches(&RequestReview, &[]);
        RequestReview.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        // The message really was posted; reporting failure would be a lie.
        assert!(out.contains("requested review"), "{out}");
        assert!(out.contains("warning"), "{out}");
    }

    #[tokio::test]
    async fn without_watch_no_reactions_are_polled() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        let (mut session, _buf) = slack_session(&server);

        let args = matches(&RequestReview, &[]);
        RequestReview.call(&mut session, &args).await.unwrap();

        let polled = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().contains("reactions.get"));
        assert!(!polled, "watch is opt-in; nothing should have been polled");
    }

    #[tokio::test]
    async fn watch_reports_an_approval() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        mount(
            &server,
            "reactions.get",
            json!({ "ok": true, "message": { "reactions": [
                { "name": "+1::skin-tone-3", "users": ["U1"] }
            ]}}),
        )
        .await;
        let (mut session, buf) = slack_session(&server);
        session.config.slack_watch_timeout = 5;

        let args = matches(&RequestReview, &["--watch"]);
        RequestReview.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("approved"), "{out}");
        assert!(out.contains("U1"), "{out}");
    }

    #[tokio::test]
    async fn a_rejection_outranks_a_simultaneous_approval() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        // Two reviewers disagree. The objection must not be outvoted silently.
        mount(
            &server,
            "reactions.get",
            json!({ "ok": true, "message": { "reactions": [
                { "name": "+1", "users": ["U1"] },
                { "name": "-1", "users": ["U2"] }
            ]}}),
        )
        .await;
        let (mut session, buf) = slack_session(&server);
        session.config.slack_watch_timeout = 5;

        let args = matches(&RequestReview, &["--watch"]);
        RequestReview.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("rejected"), "{out}");
        assert!(out.contains("U2"), "{out}");
    }

    #[tokio::test]
    async fn the_bots_own_reaction_is_not_a_review() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        // Some workspaces auto-react; that must not self-approve.
        mount(
            &server,
            "reactions.get",
            json!({ "ok": true, "message": { "reactions": [
                { "name": "+1", "users": ["UBOT"] }
            ]}}),
        )
        .await;
        let (mut session, buf) = slack_session(&server);
        session.config.slack_watch_timeout = 1;
        session.config.slack_poll_interval = 1;

        let args = matches(&RequestReview, &["--watch"]);
        RequestReview.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(out.contains("no review reaction"), "{out}");
        assert!(!out.contains("approved"), "{out}");
    }

    #[tokio::test]
    async fn watch_gives_up_after_repeated_failures_and_fails_the_command() {
        let server = MockServer::start().await;
        mount_post_path(&server).await;
        mount(
            &server,
            "reactions.get",
            json!({ "ok": false, "error": "message_not_found" }),
        )
        .await;
        let (mut session, buf) = slack_session(&server);
        session.config.slack_watch_timeout = 60;
        session.config.slack_poll_interval = 1;

        let args = matches(&RequestReview, &["--watch"]);
        let err = RequestReview.call(&mut session, &args).await.unwrap_err();

        // A watch that could never see the message is a failed command, so an
        // MCP caller gets a failed tool call rather than a hopeful summary.
        assert!(err.to_string().contains("message_not_found"), "{err}");
        assert!(
            buf.contents().contains("repeated errors"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn rate_limiting_does_not_count_as_a_failure() {
        let server = MockServer::start().await;
        Mock::given(path("/reactions.get"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;
        let slack = slack_for(&server);
        let posted = PostedMessage {
            channel: CHANNEL.to_owned(),
            ts: TS.to_owned(),
        };

        // Deadline well inside MAX_CONSECUTIVE_FAILURES worth of polls: if
        // throttling counted as failure the outcome would be Failed.
        let outcome = watch(
            &slack,
            &posted,
            Duration::from_millis(10),
            Duration::from_millis(1200),
            None,
        )
        .await;

        assert_eq!(outcome, WatchOutcome::TimedOut);
    }

    #[tokio::test]
    async fn watch_times_out_with_an_elapsed_deadline_after_one_poll() {
        let server = MockServer::start().await;
        mount(
            &server,
            "reactions.get",
            json!({ "ok": true, "message": { "reactions": [] }}),
        )
        .await;
        let slack = slack_for(&server);
        let posted = PostedMessage {
            channel: CHANNEL.to_owned(),
            ts: TS.to_owned(),
        };

        let outcome = watch(
            &slack,
            &posted,
            Duration::from_secs(60),
            Duration::from_millis(1),
            None,
        )
        .await;

        assert_eq!(outcome, WatchOutcome::TimedOut);
        // Exactly one look, even though the deadline had effectively passed.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn completion_offers_the_flags() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:2", &["h1"], "ok");
        let out = RequestReview.complete(&session, "--w", "");
        assert!(out.contains(&"--watch".to_owned()), "{out:?}");
    }

    #[test]
    fn channel_flag_overrides_the_configured_channel() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:2", &["h1"], "ok");
        session.config.slack_channel = "#configured".to_owned();
        let args = matches(&RequestReview, &["--channel", "#override"]);
        assert_eq!(resolve_channel(&session, &args).unwrap(), "#override");

        let args = matches(&RequestReview, &[]);
        assert_eq!(resolve_channel(&session, &args).unwrap(), "#configured");
    }
}
