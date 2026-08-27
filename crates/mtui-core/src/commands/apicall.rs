//! Backend-API commands (`assign`, `unassign`, `reject`, `comment`).
//!
//! Each command dispatches to the OSC or Gitea backend by the loaded report's
//! own [`UpdateSource`] — resolved at load from the template's
//! `gitea_commit_hash`, never inferred from the RRID's shape (#433: the
//! SL-Micro 6.0/6.1 cutover shares the `SLFO:1.1` id space between both
//! workflows). A Product Increment's reference-host lock is bracketed around the
//! loaded report, not these review actions: `Session::load_update_reported`
//! seeds `lock_comment`, `Target::close` releases on unload/quit. `approve`
//! lives in [`approve`](super::approve) and reuses the dispatch helpers here.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{Gitea, GiteaError, Osc, TeReGen};
use mtui_types::UpdateSource;

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Whether the loaded report is handled by the Gitea backend.
///
/// `Git` routes to Gitea, `Obs` to OSC. A Product Increment carries no Gitea
/// metadata and so always resolves `Obs`, whatever its RRID looks like.
pub(crate) fn is_gitea_workflow(session: &Session) -> bool {
    session.metadata().update_source() == UpdateSource::Git
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
/// build errors onto [`CommandError`]. Reuses the session-scoped
/// [`HttpClient`](mtui_datasources::HttpClient) via [`Gitea::with_client`],
/// while preserving [`Gitea::new`]'s empty-token guard.
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
/// [`HttpClient`](mtui_datasources::HttpClient) via [`TeReGen::with_client`].
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

/// Prints best-effort TeReGen context for the loaded update: live
/// priority/deadline from one `GET /reports/{id}`, plus who holds or has decided
/// each review group. Context only, never a gate, and infallible because it runs
/// *after* the assign succeeded — an error here would dress a successful action
/// up as a failure, so malformed payloads are filtered rather than raised. An
/// empty `assignees` map is not authoritative (a lookup failure yields the same,
/// and the endpoint is cached ~300s), so it prints nothing.
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

    // Non-list group values are skipped, non-object entries filtered, and a
    // null/missing user or state renders as '?'.
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

/// Adds the common `-g/--group` + `-u/--user` args.
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

/// Assigns a review request to a user or group.
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
        if is_gitea_workflow(session) {
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
        show_priority_deadline(session, &rrid).await;
        session.display.println(&format!("assigned {rrid}"));
        Ok(())
    }
}

// --- unassign ---------------------------------------------------------------

/// Unassigns a review request.
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
        if is_gitea_workflow(session) {
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
        session.display.println(&format!("unassigned {rrid}"));
        Ok(())
    }
}

// --- reject -----------------------------------------------------------------

/// Valid `--reason` values for `reject`.
const REJECT_REASONS: &[&str] = &[
    "admin",
    "retracted",
    "build_problem",
    "not_fixed",
    "regression",
    "false_reject",
    "tracking_issue",
];

/// Rejects a review request.
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

        if is_gitea_workflow(session) {
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
        session.display.println(&format!("rejected {rrid}"));
        Ok(())
    }
}

// --- comment ----------------------------------------------------------------

/// Adds a comment to a review request.
///
/// `-m/--message` rather than an interactive prompt, so it works headlessly.
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
                .help("The comment body (required)"),
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
        if is_gitea_workflow(session) {
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

    /// The dispatch reads the report's own `UpdateSource`, not the RRID's
    /// shape: `Git` routes to Gitea even at the dual-served `1.1`, and `Obs`
    /// routes to OSC even at `1.2`.
    #[test]
    fn is_gitea_workflow_reflects_the_reports_update_source() {
        let (mut session, _buf) = session_with_hosts("SUSE:SLFO:1.1:5", &["h1"], "ok");
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
        assert!(is_gitea_workflow(&session));

        let (mut session, _buf) = session_with_hosts("SUSE:SLFO:1.2:5", &["h1"], "ok");
        session.metadata_mut().base_mut().update_source = UpdateSource::Obs;
        assert!(!is_gitea_workflow(&session));

        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert!(!is_gitea_workflow(&session));
    }

    /// A Product Increment carries no Gitea metadata, so it always resolves
    /// `Obs`. Unlike `qam`'s precondition guard, which groups PI *with* SLFO —
    /// merging the two would route PI through the wrong backend.
    #[test]
    fn is_gitea_workflow_excludes_pi() {
        for id in ["SUSE:PI:1.1:5", "SUSE:PI:1.2:5", "SUSE:PI:42:99"] {
            let (session, _buf) = session_with_hosts(id, &["h1"], "ok");
            assert!(
                !is_gitea_workflow(&session),
                "{id} must use the OSC backend"
            );
        }
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

    // The PI reference-host lock bracket lives on report load, not on these
    // commands: `session::tests::load_update_reported_seeds_pi_lock_comment_when_enabled`
    // and its siblings pin it at that seam.

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes the
    // mutation of the process-global `$OSC_CONFIG` exclusive.
    #[allow(unsafe_code)]
    async fn osc_dispatch_maintenance_assign_runs_backend() {
        // A missing oscrc makes credential resolution fail fast offline,
        // surfacing the OSC-branch error and so proving the dispatch ran.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let args = matches(&Assign, &["-g", "qam-sle"]);
        let res = Assign.call(&mut session, &args).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        if let Err(e) = res {
            assert!(matches!(e, CommandError::Other(m) if m.contains("osc assign failed")));
        }
    }

    #[tokio::test]
    async fn assign_gitea_dispatch_uses_pr_api() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // An empty comment history (unassigned, no decision) lets the marker
        // post succeed; the bare PR GET is the catch-all fallback.
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
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();

        // Force assign skips the open-group guard.
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

        // A 500 from the PR API must surface as a CommandError, not be
        // swallowed into an empty success.
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
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
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

        // One server backs both APIs; the path-matched TeReGen mock is
        // registered first so it wins over the catch-all Gitea GET.
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
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
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

        // Neither priority nor deadline: nothing TeReGen-related may print.
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
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
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

    /// Runs `assign` against a mock whose `GET /reports/{rrid}` returns
    /// `report_body` and whose Gitea PR API succeeds, returning the display
    /// buffer contents.
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
        session.metadata_mut().base_mut().update_source = UpdateSource::Git;
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        session.config.teregen_api = server.uri();

        let args = matches(&Assign, &["--force"]);
        Assign.call(&mut session, &args).await.unwrap();
        buf.contents()
    }

    #[tokio::test]
    async fn assign_shows_existing_assignment_holders() {
        // A group may carry both a decision and a live assignment
        // (decider != tester).
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
        // assign already succeeded.
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
