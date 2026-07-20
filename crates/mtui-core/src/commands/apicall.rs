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
use mtui_datasources::{Gitea, GiteaError, Osc, TeReGen};
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
///
/// Reuses the session-scoped [`HttpClient`](mtui_datasources::HttpClient) (perf
/// bead `mtui-rs-0mop.13`) via [`Gitea::with_client`], while preserving
/// [`Gitea::new`]'s empty-token guard.
pub(crate) fn gitea_client(session: &Session) -> Result<Gitea, CommandError> {
    let apiurl = session
        .metadata()
        .giteaprapi()
        .ok_or_else(|| CommandError::Other("no Gitea PR API URL on this report".to_owned()))?
        .to_owned();
    if session.config.gitea_token.is_empty() {
        return Err(CommandError::Other(format!(
            "could not build Gitea client: {}",
            GiteaError::MissingToken
        )));
    }
    let http = session
        .http_client()
        .map_err(|e| CommandError::Other(format!("could not build Gitea client: {e}")))?;
    Gitea::with_client(
        http,
        session.config.gitea_token.clone(),
        session.config.session_user.clone(),
        &apiurl,
        &session.config.gitea_url,
        None,
    )
    .map_err(|e| CommandError::Other(format!("could not build Gitea client: {e}")))
}

/// Builds a TeReGen client for the loaded report, reusing the session-scoped
/// [`HttpClient`](mtui_datasources::HttpClient) (perf bead `mtui-rs-0mop.13`)
/// via [`TeReGen::with_client`].
///
/// # Errors
///
/// [`CommandError::Other`] when the shared HTTP client cannot be built.
pub(crate) fn teregen_client(session: &Session) -> Result<TeReGen, CommandError> {
    let http = session
        .http_client()
        .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
    Ok(TeReGen::with_client(http, &session.config.teregen_api))
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
            session.metadata_mut().base_mut().lock_comment = comment.clone();
            tracing::info!("Locking reference hosts for {rrid}");
            session.targets_mut().lock(&comment).await;
        }
        PiAction::Unlock => {
            tracing::info!("Unlocking reference hosts for {rrid}");
            session.targets_mut().unlock().await;
            session.metadata_mut().base_mut().lock_comment = String::new();
        }
        PiAction::None => {}
    }
}

/// Prints best-effort TeReGen context for the loaded update
/// (upstream `BaseApiCall._show_priority_deadline`).
///
/// Sourced from a single `GET /reports/{id}` fetch: the live priority/deadline
/// and, when the report already carries assignment state, who currently holds
/// or has decided each review group.
///
/// Context only, never a gate. It runs **after** the assign has already
/// succeeded, so it is fully infallible: silent when TeReGen has nothing (or is
/// unreachable), and malformed payloads are filtered rather than raised — a
/// panic here would dress a successful action up as an error. An empty
/// `assignees` map is not authoritative (it is also what an upstream lookup
/// failure yields, and the server caches this endpoint for ~300s), so its
/// absence prints nothing.
async fn show_priority_deadline(session: &mut Session, rrid: &mtui_types::RequestReviewID) {
    let teregen = match teregen_client(session) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("could not build TeReGen client: {e}");
            return;
        }
    };
    let Some(info) = teregen.info(&rrid.to_string()).await else {
        return;
    };

    let priority = info.get("priority").and_then(serde_json::Value::as_i64);
    let deadline = info
        .get("deadline")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    if priority.is_some() || deadline.is_some() {
        let p = priority.map_or_else(|| "?".to_owned(), |v| v.to_string());
        let d = deadline.unwrap_or("?");
        session
            .display
            .println(&format!("TeReGen: priority {p}, deadline {d}"));
    }

    // Best-effort assignment context: warn when someone already holds (or has
    // decided) a review group. Non-list group values are skipped, non-object
    // entries filtered, and a null/missing user or state renders as '?'.
    let Some(assignees) = info.get("assignees").and_then(serde_json::Value::as_object) else {
        return;
    };
    let mut groups: Vec<(&String, &serde_json::Value)> = assignees.iter().collect();
    groups.sort_by(|a, b| a.0.cmp(b.0));
    for (group, entries) in groups {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        let holders = entries
            .iter()
            .filter_map(serde_json::Value::as_object)
            .map(|e| {
                let user = e
                    .get("user")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let state = e
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                format!("{user} ({state})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        if !holders.is_empty() {
            session.display.println(&format!(
                "TeReGen: {group} assignment state (may lag ~5 min): {holders}"
            ));
        }
    }
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
            gitea
                .assign(user_override(args).as_deref(), args.get_flag("force"))
                .await
                .map_err(|e| CommandError::Other(format!("gitea assign failed: {e}")))?;
        } else {
            tracing::info!("Assign request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.assign(&groups(args))
                .await
                .map_err(|e| CommandError::Other(format!("osc assign failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Lock).await;
        show_priority_deadline(session, &rrid).await;
        session.display.println(&format!("assigned {rrid}"));
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
            gitea
                .unassign(user_override(args).as_deref())
                .await
                .map_err(|e| CommandError::Other(format!("gitea unassign failed: {e}")))?;
        } else {
            tracing::info!("Unassign request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.unassign(&groups(args))
                .await
                .map_err(|e| CommandError::Other(format!("osc unassign failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Unlock).await;
        session.display.println(&format!("unassigned {rrid}"));
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
            gitea
                .reject(&reason, user_override(args).as_deref(), &message)
                .await
                .map_err(|e| CommandError::Other(format!("gitea reject failed: {e}")))?;
        } else {
            tracing::info!("Reject request {}", rrid.review_id);
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.reject(&groups(args), &reason, &message)
                .await
                .map_err(|e| CommandError::Other(format!("osc reject failed: {e}")))?;
        }
        pi_autolock(session, PiAction::Unlock).await;
        session.display.println(&format!("rejected {rrid}"));
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
            gitea
                .comment(&comment)
                .await
                .map_err(|e| CommandError::Other(format!("gitea comment failed: {e}")))?;
        } else {
            let osc = Osc::new(session.config.clone(), rrid.clone());
            osc.comment(&comment)
                .await
                .map_err(|e| CommandError::Other(format!("osc comment failed: {e}")))?;
        }
        session
            .display
            .println(&format!("comment posted on {rrid}"));
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
        session.metadata_mut().base_mut().rrid = "SUSE:PI:1.2:5".parse().ok();

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
        session.metadata_mut().base_mut().rrid = "SUSE:PI:1.2:5".parse().ok();
        pi_autolock(&mut session, PiAction::Lock).await;
        assert_eq!(session.metadata().base().lock_comment, "");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `std::env::set_var`/`remove_var` are `unsafe` in edition 2024; the
    // `#[serial(osc_config_env)]` guard makes the mutation exclusive.
    #[allow(unsafe_code)]
    async fn osc_dispatch_maintenance_assign_runs_backend() {
        // A Maintenance RRID routes to the native OBS backend. Point $OSC_CONFIG
        // at an oscrc that does not exist so credential resolution fails fast
        // (offline, no network), surfacing the OSC-branch error and exercising
        // that dispatch. `$OSC_CONFIG` is process-global, hence `#[serial]`.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // SAFETY: serialised via `#[serial(osc_config_env)]` so no other test
        // reads/writes this env var concurrently.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let args = matches(&Assign, &["-g", "qam-sle"]);
        let res = Assign.call(&mut session, &args).await;
        // SAFETY: still inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        if let Err(e) = res {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc assign failed")));
        }
    }

    #[tokio::test]
    async fn assign_gitea_dispatch_uses_pr_api() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A SLFO report routes to Gitea; the comments GET returns an empty
        // history (unassigned, no decision) so `assign --force` posts the marker
        // and succeeds. The PR GET is the catch-all fallback.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/comments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();

        // Force assign skips the open-group guard; the mock accepts the marker
        // post, so the Gitea call succeeds and the command confirms.
        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("assigned SUSE:SLFO:1.2:5"),
            "expected success confirmation, got: {}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn comment_gitea_failure_is_surfaced() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A SLFO report routes to Gitea; the PR API returns 500 so the Gitea
        // call fails. The failure must be surfaced as a CommandError, not
        // swallowed into an Ok/empty success.
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

        let args = matches(&Comment, &["-m", "hi"]);
        let err = Comment.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("gitea comment failed")));
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
            .and(path_regex(r"/comments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
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
            .and(path_regex(r"/comments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
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

    /// Runs `assign` against a mock server whose `GET /reports/{rrid}` returns
    /// `report_body`, with the Gitea PR API stubbed to succeed, and returns the
    /// display buffer contents. Mirrors the wiremock harness of the
    /// priority/deadline tests above.
    async fn assign_with_report(report_body: serde_json::Value) -> String {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/reports/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(report_body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"/comments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
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
        session.metadata_mut().base_mut().giteaprapi = Some(server.uri());
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        session.config.teregen_api = server.uri();

        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();
        buf.contents()
    }

    #[tokio::test]
    async fn assign_shows_existing_assignment_holders() {
        // Current holders (and past decisions) are surfaced; a group may carry
        // both a decision entry and a live assignment (decider != tester).
        let out = assign_with_report(serde_json::json!({
            "priority": 700,
            "deadline": "2026-07-09",
            "assignees": {
                "qam-sle": [
                    {"user": "pluskalm", "state": "accepted"},
                    {"user": "mpluskal", "state": "assigned"},
                ]
            }
        }))
        .await;
        assert!(
            out.contains(
                "TeReGen: qam-sle assignment state (may lag ~5 min): \
                 pluskalm (accepted), mpluskal (assigned)"
            ),
            "expected holders line, got: {out}"
        );
    }

    #[tokio::test]
    async fn assign_empty_assignees_map_prints_nothing() {
        // An empty map is not authoritative (also what a lookup failure yields),
        // so it stays silent and never gates the action.
        let out = assign_with_report(serde_json::json!({
            "priority": 700,
            "deadline": "2026-07-09",
            "assignees": {}
        }))
        .await;
        assert!(!out.contains("assignment state"), "got: {out}");
        assert!(out.contains("TeReGen: priority 700"), "got: {out}");
    }

    #[tokio::test]
    async fn assign_malformed_assignees_never_breaks_the_flow() {
        // Malformed payloads are filtered, never raised — this prints after the
        // assign already succeeded. Non-list group values are skipped, non-dict
        // entries filtered, and an explicit null user/state renders as '?'.
        let out = assign_with_report(serde_json::json!({
            "assignees": {
                "a": null,
                "b": ["not-a-dict"],
                "c": [{"user": null, "state": "assigned"}],
                "d": [{"user": "bob", "state": "assigned"}],
            }
        }))
        .await;
        assert!(
            out.contains("c assignment state (may lag ~5 min): ? (assigned)"),
            "got: {out}"
        );
        assert!(
            out.contains("d assignment state (may lag ~5 min): bob (assigned)"),
            "got: {out}"
        );
        assert!(!out.contains("not-a-dict"), "got: {out}");
    }
}
