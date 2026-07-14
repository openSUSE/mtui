//! Backend-API commands (`assign`, `unassign`, `reject`, `comment`).
//!
//! Ports upstream `mtui.commands.apicall`. Each command dispatches to the OSC or
//! Gitea backend depending on the loaded request's kind (Gitea for SLFO with a
//! maintenance id other than `1.1`, OSC otherwise), and — for a Product
//! Increment with `lock_pi_autolock` — locks/unlocks the reference hosts around
//! the action. `approve` lives in [`approve`](super::approve) and reuses the
//! dispatch helpers here.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{Gitea, Osc, TeReGen};
use mtui_types::RequestKind;

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The PI host-lock action a command performs around its backend call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiAction {
    /// Lock reference hosts (upstream `_pi_action = "lock"`, e.g. `assign`).
    Lock,
    /// Unlock reference hosts (upstream `"unlock"`, e.g. `unassign`/`reject`).
    Unlock,
    /// Neither (upstream `None`, e.g. `comment`).
    None,
}

/// Whether the loaded request is handled by the Gitea backend (upstream
/// `_is_gitea_workflow`): SLFO with a maintenance id other than `1.1`.
pub(crate) fn is_gitea_workflow(rrid: &mtui_types::RequestReviewID) -> bool {
    rrid.kind == RequestKind::Slfo && rrid.maintenance_id != "1.1"
}

/// The `-g/--group` values (repeatable), defaulting to an empty slice.
fn groups(args: &ArgMatches) -> Vec<String> {
    args.get_many::<String>("group")
        .map(|it| it.cloned().collect())
        .unwrap_or_default()
}

/// The `-u/--user` Gitea override, or `None` when unset/empty.
fn user_override(args: &ArgMatches) -> Option<String> {
    args.get_one::<String>("user")
        .filter(|s| !s.is_empty())
        .cloned()
}

/// Builds a Gitea client for the loaded report, mapping the missing-PR-URL and
/// build errors onto [`CommandError`].
pub(crate) fn gitea_client(session: &Session) -> Result<Gitea, CommandError> {
    let apiurl = session
        .metadata()
        .giteaprapi()
        .ok_or_else(|| CommandError::Other("no Gitea PR API URL on this report".to_owned()))?
        .to_owned();
    Gitea::new(&session.config, &apiurl, None)
        .map_err(|e| CommandError::Other(format!("could not build Gitea client: {e}")))
}

/// Locks/unlocks the reference hosts around a PI action (upstream `_pi_autolock`).
///
/// No-op unless the request is a PI, `lock_pi_autolock` is enabled, and the
/// command declares a lock/unlock action.
pub(crate) async fn pi_autolock(session: &mut Session, action: PiAction) {
    if action == PiAction::None || !session.config.lock_pi_autolock {
        return;
    }
    let Some(rrid) = session.metadata().rrid().cloned() else {
        return;
    };
    if rrid.kind != RequestKind::Pi {
        return;
    }
    match action {
        PiAction::Lock => {
            let comment = format!("testing of {rrid}");
            session.templates.active_mut().base_mut().lock_comment = comment.clone();
            tracing::info!("Locking reference hosts for {rrid}");
            session.targets_mut().lock(&comment).await;
        }
        PiAction::Unlock => {
            tracing::info!("Unlocking reference hosts for {rrid}");
            session.targets_mut().unlock().await;
            session.templates.active_mut().base_mut().lock_comment = String::new();
        }
        PiAction::None => {}
    }
}

/// Prints the loaded update's priority + deadline from TeReGen, if available
/// (upstream `BaseApiCall._show_priority_deadline`).
///
/// Best-effort context for the tester picking up an update: silent when TeReGen
/// has nothing for this request (both values `None`) or is unreachable. A
/// failure to build the client is logged and ignored — it never fails the
/// command.
async fn show_priority_deadline(session: &mut Session, rrid: &mtui_types::RequestReviewID) {
    let teregen = match TeReGen::new(&session.config, &session.config.teregen_api) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("could not build TeReGen client: {e}");
            return;
        }
    };
    let (priority, deadline) = teregen.priority_deadline(&rrid.to_string()).await;
    if priority.is_none() && deadline.is_none() {
        return;
    }
    let p = priority.map_or_else(|| "?".to_owned(), |v| v.to_string());
    let d = deadline.unwrap_or_else(|| "?".to_owned());
    session
        .display
        .println(&format!("TeReGen: priority {p}, deadline {d}"));
}

/// Adds the common `-g/--group` + `-u/--user` args (upstream base
/// `_add_arguments`).
fn add_common_args(cmd: clap::Command) -> clap::Command {
    cmd.arg(
        Arg::new("group")
            .short('g')
            .long("group")
            .value_name("GROUP")
            .action(ArgAction::Append)
            .help("Group to act on (not valid for the Gitea workflow)"),
    )
    .arg(
        Arg::new("user")
            .short('u')
            .long("user")
            .value_name("USER")
            .default_value("")
            .help("User override for the Gitea workflow (Gitea only)"),
    )
}

/// Common tab completion for the backend-API commands.
fn common_complete(session: &Session, text: &str, extra: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = ["-g", "--group", "-u", "--user"]
        .iter()
        .chain(extra.iter())
        .filter(|f| f.starts_with(text))
        .map(|s| (*s).to_owned())
        .collect();
    out.extend(template_completion(session, text));
    out
}

// --- assign -----------------------------------------------------------------

/// Assigns a review request to a user or group (upstream `Assign`).
pub struct Assign;

#[async_trait]
impl Command for Assign {
    fn name(&self) -> &'static str {
        "assign"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Assigns a review request to a user or group.")
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_common_args(cmd).arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::SetTrue)
                .help("Force assign the review in Gitea even without an open group"),
        )
    }
    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        common_complete(session, text, &["-f", "--force"])
    }
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        if is_gitea_workflow(&rrid) {
            let gitea = gitea_client(session)?;
            if let Err(e) = gitea
                .assign(user_override(args).as_deref(), args.get_flag("force"))
                .await
            {
                tracing::error!("{e}");
            }
        } else {
            tracing::info!("Assign request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.assign(&groups(args))
                .await
                .map_err(|e| CommandError::Other(format!("osc assign failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Lock).await;
        show_priority_deadline(session, &rrid).await;
        Ok(())
    }
}

// --- unassign ---------------------------------------------------------------

/// Unassigns a review request (upstream `Unassign`).
pub struct Unassign;

#[async_trait]
impl Command for Unassign {
    fn name(&self) -> &'static str {
        "unassign"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Unassigns a review request.")
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_common_args(cmd)
    }
    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        common_complete(session, text, &[])
    }
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        if is_gitea_workflow(&rrid) {
            let gitea = gitea_client(session)?;
            if let Err(e) = gitea.unassign(user_override(args).as_deref()).await {
                tracing::error!("{e}");
            }
        } else {
            tracing::info!("Unassign request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.unassign(&groups(args))
                .await
                .map_err(|e| CommandError::Other(format!("osc unassign failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Unlock).await;
        Ok(())
    }
}

// --- reject -----------------------------------------------------------------

/// Valid `--reason` values for `reject` (upstream `choices`).
const REJECT_REASONS: &[&str] = &[
    "admin",
    "retracted",
    "build_problem",
    "not_fixed",
    "regression",
    "false_reject",
    "tracking_issue",
];

/// Rejects a review request (upstream `Reject`).
pub struct Reject;

#[async_trait]
impl Command for Reject {
    fn name(&self) -> &'static str {
        "reject"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Rejects a review request.")
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_common_args(cmd)
            .arg(
                Arg::new("reason")
                    .short('r')
                    .long("reason")
                    .required(true)
                    .value_parser(clap::builder::PossibleValuesParser::new(REJECT_REASONS))
                    .help("Reason to reject the update (required)"),
            )
            .arg(
                Arg::new("message")
                    .short('m')
                    .long("message")
                    .num_args(0..)
                    .action(ArgAction::Append)
                    .help("Rejection message (takes the remainder of the command)"),
            )
    }
    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        common_complete(session, text, &["-r", "--reason", "-m", "--message"])
    }
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let reason = args
            .get_one::<String>("reason")
            .cloned()
            .unwrap_or_default();
        let message = args
            .get_many::<String>("message")
            .map(|it| it.cloned().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();

        if is_gitea_workflow(&rrid) {
            let gitea = gitea_client(session)?;
            if let Err(e) = gitea
                .reject(&reason, user_override(args).as_deref(), &message)
                .await
            {
                tracing::error!("{e}");
            }
        } else {
            tracing::info!("Reject request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.reject(&groups(args), &reason, &message)
                .await
                .map_err(|e| CommandError::Other(format!("osc reject failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Unlock).await;
        Ok(())
    }
}

// --- comment ----------------------------------------------------------------

/// Adds a comment to a review request (upstream `Comment`).
///
/// Deviation from upstream: upstream prompts interactively (`ask_user`). The
/// interactive prompt is a Phase-6 REPL concern; here the comment is supplied
/// via `-m/--message` so the command works headlessly (MCP) and in the REPL.
pub struct Comment;

#[async_trait]
impl Command for Comment {
    fn name(&self) -> &'static str {
        "comment"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Adds a comment to a review request.")
    }
    fn scope(&self) -> Scope {
        Scope::Fanout
    }
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("message")
                .short('m')
                .long("message")
                .num_args(1..)
                .action(ArgAction::Append)
                .help("The comment body (required; interactive prompt lands in Phase 6)"),
        )
    }
    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = ["-m", "--message"]
            .iter()
            .filter(|f| f.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect();
        out.extend(template_completion(session, text));
        out
    }
    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let comment = args
            .get_many::<String>("message")
            .map(|it| it.cloned().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        if comment.is_empty() {
            return Err(CommandError::Other(
                "a comment is required (use -m/--message)".to_owned(),
            ));
        }
        if is_gitea_workflow(&rrid) {
            let gitea = gitea_client(session)?;
            if let Err(e) = gitea.comment(&comment).await {
                tracing::error!("{e}");
            }
        } else {
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.comment(&comment)
                .await
                .map_err(|e| CommandError::Other(format!("osc comment failed: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn names_and_fanout_scopes() {
        assert_eq!(Assign.name(), "assign");
        assert_eq!(Unassign.name(), "unassign");
        assert_eq!(Reject.name(), "reject");
        assert_eq!(Comment.name(), "comment");
        for c in [
            Assign.scope(),
            Unassign.scope(),
            Reject.scope(),
            Comment.scope(),
        ] {
            assert_eq!(c, Scope::Fanout);
        }
    }

    #[test]
    fn is_gitea_workflow_matches_upstream() {
        let slfo: mtui_types::RequestReviewID = "SUSE:SLFO:1.2:5".parse().unwrap();
        assert!(is_gitea_workflow(&slfo));
        let slfo_11: mtui_types::RequestReviewID = "SUSE:SLFO:1.1:5".parse().unwrap();
        assert!(!is_gitea_workflow(&slfo_11));
        let maint: mtui_types::RequestReviewID = "SUSE:Maintenance:1:1".parse().unwrap();
        assert!(!is_gitea_workflow(&maint));
    }

    #[test]
    fn reject_requires_reason_and_validates_choices() {
        let cmd = Reject.configure(clap::Command::new("reject").no_binary_name(true));
        assert!(cmd.clone().try_get_matches_from([] as [&str; 0]).is_err());
        assert!(cmd.clone().try_get_matches_from(["-r", "bogus"]).is_err());
        assert!(cmd.try_get_matches_from(["-r", "regression"]).is_ok());
    }

    #[test]
    fn assign_completion_includes_force() {
        let (session, _buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        let out = Assign.complete(&session, "-f", "");
        assert_eq!(out, vec!["-f"]);
    }

    #[tokio::test]
    async fn each_command_errors_without_report() {
        let (mut session, _buf) = empty_session();
        for (cmd, argv) in [
            (&Assign as &dyn Command, vec![]),
            (&Unassign as &dyn Command, vec![]),
            (&Reject as &dyn Command, vec!["-r", "regression"]),
            (&Comment as &dyn Command, vec!["-m", "hi"]),
        ] {
            let args = matches(cmd, &argv);
            assert!(matches!(
                cmd.call(&mut session, &args).await.unwrap_err(),
                CommandError::Other(_)
            ));
        }
    }

    #[tokio::test]
    async fn comment_requires_message() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Comment, &[]);
        let err = Comment.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("comment is required")));
    }

    #[tokio::test]
    async fn pi_autolock_locks_and_unlocks_pi_hosts() {
        // A PI request with lock_pi_autolock enabled locks on Lock and clears
        // the comment on Unlock; a None action is a no-op.
        let (mut session, _buf) = session_with_hosts("SUSE:PI:1.2:5", &["h1"], "ok");
        session.config.lock_pi_autolock = true;
        session.templates.active_mut().base_mut().rrid = "SUSE:PI:1.2:5".parse().ok();

        pi_autolock(&mut session, PiAction::None).await;
        assert_eq!(session.metadata().base().lock_comment, "");

        pi_autolock(&mut session, PiAction::Lock).await;
        assert_eq!(
            session.metadata().base().lock_comment,
            "testing of SUSE:PI:1.2:5"
        );

        pi_autolock(&mut session, PiAction::Unlock).await;
        assert_eq!(session.metadata().base().lock_comment, "");
    }

    #[tokio::test]
    async fn pi_autolock_skips_non_pi_and_disabled() {
        // Not a PI → no-op even with a lock action.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config.lock_pi_autolock = true;
        pi_autolock(&mut session, PiAction::Lock).await;
        assert_eq!(session.metadata().base().lock_comment, "");

        // PI but the knob is off → no-op.
        let (mut session, _buf) = session_with_hosts("SUSE:PI:1.2:5", &["h1"], "ok");
        session.config.lock_pi_autolock = false;
        session.templates.active_mut().base_mut().rrid = "SUSE:PI:1.2:5".parse().ok();
        pi_autolock(&mut session, PiAction::Lock).await;
        assert_eq!(session.metadata().base().lock_comment, "");
    }

    #[tokio::test]
    async fn osc_dispatch_maintenance_assign_runs_backend() {
        // A Maintenance RRID routes to the native OBS backend. Point it at an
        // oscrc that does not exist so credential resolution fails fast (offline,
        // no network), surfacing the OSC-branch error and exercising that dispatch.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.config.obs_conffile = "/nonexistent/oscrc-for-tests".to_owned();
        let args = matches(&Assign, &["-g", "qam-sle"]);
        if let Err(e) = Assign.call(&mut session, &args).await {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc assign failed")));
        }
    }

    #[tokio::test]
    async fn assign_gitea_dispatch_uses_pr_api() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A SLFO report routes to Gitea; point the PR API at a mock that accepts
        // the review-request lookup + assignment marker post.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "requested_reviewers": [],
                "state": "open",
                "head": {"sha": "abc"}
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

        // Force assign skips the open-group guard; a Gitea error is logged, not
        // returned — so the command completes Ok regardless of the mock's exact
        // marker bookkeeping.
        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn assign_surfaces_teregen_priority_deadline() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // One server backs both the Gitea PR API and the TeReGen report API.
        // The TeReGen `GET /reports/{rrid}` mock is registered first and matched
        // by path, so it wins over the catch-all Gitea GET.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/reports/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "priority": 700,
                "deadline": "2026-08-01T00:00:00Z"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "requested_reviewers": [],
                "state": "open",
                "head": {"sha": "abc"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.templates.active_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_token = "tok".to_owned();
        session.config.teregen_api = server.uri();

        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();

        assert!(
            buf.contents()
                .contains("TeReGen: priority 700, deadline 2026-08-01T00:00:00Z"),
            "expected priority/deadline line, got: {}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn assign_silent_when_teregen_has_no_priority_deadline() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // TeReGen returns a report object with neither priority nor deadline →
        // the assign must print nothing TeReGen-related.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/reports/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "requested_reviewers": [],
                "state": "open",
                "head": {"sha": "abc"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let (mut session, buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.templates.active_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_token = "tok".to_owned();
        session.config.teregen_api = server.uri();

        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();

        assert!(
            !buf.contents().contains("TeReGen:"),
            "expected no TeReGen line, got: {}",
            buf.contents()
        );
    }
}
