//! The `updates` command — list the update queue via the TeReGen API.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use mtui_datasources::{TeReGen, UpdatesQuery};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The `--status` value that widens the queue to every status (upstream
/// `Updates.STATUS_ALL`).
const STATUS_ALL: &str = "all";

/// Lists the update queue (unassigned + in-testing by default), fetched live
/// from the TeReGen API.
///
/// Ports upstream `mtui.commands.updates.Updates`. The default view is the
/// actionable pickup queue: **unassigned** updates that are **in testing**.
/// `--assignee`/`--mine`/`--all-assignees` pick another assignment view (and
/// drop the unassigned default); `--status all` widens to every status. This is
/// a session-global query, so it runs exactly once ([`Scope::Single`]) rather
/// than fanning out per template.
pub struct Updates;

#[async_trait]
impl Command for Updates {
    fn name(&self) -> &'static str {
        "updates"
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("review_group")
                .long("review-group")
                .value_name("GROUP")
                .help("filter by review group, e.g. qam-sle"),
        )
        .arg(
            Arg::new("status")
                .long("status")
                .value_name("STATUS")
                .default_value("testing")
                .help("filter by status (default: testing); use 'all' for every status"),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .default_value("0")
                .help("cap the number of rows (0 = all)"),
        )
        .arg(
            Arg::new("assignee")
                .long("assignee")
                .value_name("USER")
                .help("filter to updates assigned to this user (any qam group)"),
        )
        .arg(
            Arg::new("mine")
                .long("mine")
                .action(ArgAction::SetTrue)
                .help("filter to updates assigned to the current session user"),
        )
        .arg(
            Arg::new("all_assignees")
                .long("all-assignees")
                .action(ArgAction::SetTrue)
                .help(
                    "show every update regardless of assignee, overriding the unassigned default",
                ),
        )
        .group(
            ArgGroup::new("assignment")
                .args(["assignee", "mine", "all_assignees"])
                .multiple(false),
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let review_group = args.get_one::<String>("review_group").cloned();
        let status_arg = args
            .get_one::<String>("status")
            .cloned()
            .unwrap_or_else(|| "testing".to_owned());
        let limit = args.get_one::<usize>("limit").copied().unwrap_or(0);
        let mine = args.get_flag("mine");
        let all_assignees = args.get_flag("all_assignees");

        let assignee = if mine {
            Some(session.config.session_user.clone())
        } else {
            args.get_one::<String>("assignee").cloned()
        };

        // '--status all' is the escape hatch: widen to every status by sending
        // no status filter (the server returns released updates too).
        let status_all = status_arg == STATUS_ALL;
        let status = if status_all { None } else { Some(status_arg) };

        // Default view is the unassigned pickup queue; --assignee/--mine and
        // --all-assignees/--status all opt out of that filter.
        let chose_other_view = assignee.is_some() || all_assignees;
        let unassigned = !chose_other_view && !status_all;
        // Show the assignee column whenever assignment is part of the view.
        let want_assignment = assignee.is_some() || unassigned || all_assignees;

        let teregen = TeReGen::new(&session.config, &session.config.teregen_api)
            .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
        let query = UpdatesQuery {
            review_group: review_group.as_deref(),
            status: status.as_deref(),
            assignee: assignee.as_deref(),
            unassigned,
            with_assignment: want_assignment,
            no_cache: false,
        };
        let updates = teregen.updates(&query).await;
        let rows = updates.as_ref().and_then(serde_json::Value::as_array);
        let rows = match rows {
            Some(r) if !r.is_empty() => r,
            _ => {
                session.display.println("No updates in the queue");
                return Ok(());
            }
        };

        let shown: &[serde_json::Value] = if limit > 0 && limit < rows.len() {
            &rows[..limit]
        } else {
            rows
        };

        session
            .display
            .println(&format!("Update queue ({}):", shown.len()));
        for u in shown {
            session.display.println(&render_row(u, want_assignment));
        }
        Ok(())
    }
}

/// Renders one queue row, mirroring upstream's fixed-width layout.
fn render_row(u: &serde_json::Value, want_assignment: bool) -> String {
    let Some(obj) = u.as_object() else {
        return format!("  {u}");
    };
    let field = |k: &str| {
        obj.get(k)
            .map(json_scalar)
            .unwrap_or_else(|| "?".to_owned())
    };
    // deadline is an ISO timestamp; the date alone is enough for a row.
    let deadline = obj
        .get("deadline")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map_or_else(|| "-".to_owned(), |s| s.chars().take(10).collect());

    let mut row = format!(
        "  prio={:<5} {:<10} {:<12} {:<11} {}",
        field("priority"),
        field("status"),
        field("kind"),
        deadline,
        field("id"),
    );
    if want_assignment {
        let assignee = obj
            .get("assignee")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unassigned");
        row.push_str(&format!(" assignee={assignee}"));
    }
    row
}

/// Renders a JSON scalar the way upstream's `str()` interpolation would.
fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "?".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};
    use mtui_config::Config;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Updates.name(), "updates");
        assert_eq!(Updates.scope(), Scope::Single);
    }

    #[test]
    fn assignment_flags_are_mutually_exclusive() {
        let base = clap::Command::new("updates").no_binary_name(true);
        let cmd = Updates.configure(base);
        assert!(
            cmd.clone()
                .try_get_matches_from(["--mine", "--all-assignees"])
                .is_err()
        );
        // A single assignment view parses fine.
        assert!(cmd.try_get_matches_from(["--mine"]).is_ok());
    }

    #[test]
    fn render_row_includes_assignee_only_when_wanted() {
        let u = serde_json::json!({
            "priority": 3, "status": "testing", "kind": "Maintenance",
            "deadline": "2026-07-10T00:00:00", "id": "SUSE:Maintenance:1:1",
            "assignee": "alice"
        });
        let with = render_row(&u, true);
        assert!(with.contains("prio=3"), "{with}");
        assert!(with.contains("2026-07-10"), "{with}");
        assert!(with.contains("assignee=alice"), "{with}");
        let without = render_row(&u, false);
        assert!(!without.contains("assignee="), "{without}");
    }

    #[test]
    fn render_row_unassigned_and_missing_deadline() {
        let u = serde_json::json!({
            "priority": 1, "status": "testing", "kind": "SLFO",
            "id": "SUSE:SLFO:1.2:5"
        });
        let row = render_row(&u, true);
        assert!(row.contains("assignee=unassigned"), "{row}");
        assert!(
            row.contains(" - "),
            "missing deadline should render '-': {row}"
        );
    }

    #[tokio::test]
    async fn empty_queue_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": []})),
            )
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        session.config = config;

        let args = matches(&Updates, &[]);
        Updates.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("No updates in the queue"));
    }

    #[tokio::test]
    async fn mine_uses_session_user_and_limits_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("assignee", "tester"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": [
                {"priority": 1, "status": "testing", "kind": "Maintenance", "id": "a", "assignee": "tester"},
                {"priority": 2, "status": "testing", "kind": "Maintenance", "id": "b", "assignee": "tester"},
            ]})))
            .mount(&server)
            .await;

        let (mut session, buf) = empty_session();
        let mut config = Config::default();
        config.teregen_api = server.uri();
        config.session_user = "tester".to_owned();
        session.config = config;

        let args = matches(&Updates, &["--mine", "--limit", "1"]);
        Updates.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Update queue (1):"), "{out}");
        assert!(out.contains("assignee=tester"), "{out}");
    }
}
