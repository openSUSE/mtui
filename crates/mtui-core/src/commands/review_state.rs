//! The `review_state` command — renders [`crate::review`] for one or more
//! updates, without loading them.

use std::time::Duration;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use futures::stream::{self, StreamExt};
use mtui_datasources::sanitize_url;
use serde::Serialize;

use crate::command::{Command, Scope};
use crate::commands::support::one_line;
use crate::error::{CommandError, CommandResult};
use crate::review::{
    Failure, Field, GroupReview, Owner, ReadContext, TEXT_LIMIT, UpdateReview, parse_target,
    read_update,
};
use crate::session::Session;

use super::updates::{MineWording, resolve_mine};

/// The wall-clock budget of one source's read for one update; a read past it
/// is a failure of that source alone.
const SOURCE_BUDGET: Duration = Duration::from_secs(120);
/// Updates read at once.
const CONCURRENCY: usize = 4;
/// Head characters the table shows (`--json` carries the whole sha).
const HEAD_WIDTH: usize = 12;

const REVIEW_STATE_MINE: MineWording = MineWording {
    only: "only {login} counts as mine",
    remedy: "fix the oscrc or the [gitea] token, or drop --mine",
    no_token: "fix the oscrc or set a [gitea] token, or drop --mine",
};

/// Reads, for each update, who holds its QA review, whether that review's
/// verdict is still owed, and whether its template matches the update —
/// without loading it, assigning it or regenerating anything. Session-level,
/// hence [`Scope::Single`].
pub struct ReviewState;

#[async_trait]
impl Command for ReviewState {
    fn name(&self) -> &'static str {
        "review_state"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Shows who holds an update's QA review, whether its verdict is owed and whether \
             its template is current, without loading it.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        false
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.after_help(
            "The SLFO gate is read from the pull request's reviews, while approve and assign \
             judge \"already decided\" from decision comments and the requested reviewers, so \
             the two can disagree on one pull request. BASE is the pull request's base branch \
             as Gitea names it; mtui does not check it against the RRID.",
        )
        .arg(
            Arg::new("update")
                .num_args(1..)
                .required(true)
                .action(ArgAction::Append)
                .value_name("RRID|PR-URL")
                .help(
                    "an RRID (long or short form) or a Gitea pull request URL; repeatable. An \
                     SLFO RRID's pull request is the one its published document names; a pull \
                     request URL names no RRID, so its template column is n/a",
                ),
        )
        .arg(
            Arg::new("mine")
                .long("mine")
                .action(ArgAction::SetTrue)
                .help(
                    "add a mine column: whether the owner is your oscrc login for [obs] \
                     api_url or, with a [gitea] token, its Gitea login",
                ),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help(
                    "print {\"rows\": [...], \"failed\": [...], \"notes\": [...]} instead of the \
                     table; still fails when a lookup failed",
                ),
        )
    }

    fn complete(&self, _session: &Session, text: &str, line: &str) -> Vec<String> {
        super::support::complete_choices(&[&["--mine"], &["--json"]], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        self.call_with_budget(session, args, SOURCE_BUDGET).await
    }
}

/// Whether an update's owner is one of the `--mine` logins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mine {
    Yes,
    No,
    Unknown,
}

impl std::fmt::Display for Mine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "?",
        })
    }
}

/// The review groups the table shows: testers, not automation.
fn testers(review: &UpdateReview) -> Option<Vec<&GroupReview>> {
    match &review.reviews {
        Field::Known(all) => Some(all.iter().filter(|g| !g.automated).collect()),
        _ => None,
    }
}

fn mine_of(review: &UpdateReview, logins: &[String]) -> Mine {
    let Some(groups) = testers(review) else {
        return Mine::Unknown;
    };
    if groups
        .iter()
        .any(|g| matches!(&g.owner, Owner::Known { login } if logins.contains(login)))
    {
        Mine::Yes
    } else if groups
        .iter()
        .any(|g| matches!(g.owner, Owner::Unknown | Owner::Disputed { .. }))
    {
        Mine::Unknown
    } else {
        Mine::No
    }
}

#[derive(Serialize)]
struct JsonRow<'a> {
    #[serde(flatten)]
    review: &'a UpdateReview,
    #[serde(skip_serializing_if = "Option::is_none")]
    mine: Option<Mine>,
}

impl ReviewState {
    /// The [`Command::call`] body with the per-source budget injected, so a
    /// test can time a source out without waiting minutes.
    pub(crate) async fn call_with_budget(
        &self,
        session: &mut Session,
        args: &ArgMatches,
        budget: Duration,
    ) -> CommandResult {
        let raw: Vec<String> = args
            .get_many::<String>("update")
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        // Every argument is checked before any request.
        let targets = raw
            .iter()
            .map(|r| parse_target(r))
            .collect::<Result<Vec<_>, _>>()
            .map_err(CommandError::Other)?;
        let as_json = args.get_flag("json");
        let mut notes = Vec::new();
        let logins = if args.get_flag("mine") {
            let mine = resolve_mine(session, &REVIEW_STATE_MINE).await?;
            notes.extend(mine.note);
            Some(mine.logins)
        } else {
            None
        };
        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build an HTTP client: {e}")))?;
        let ctx = ReadContext {
            config: session.config.clone(),
            http,
            budget,
        };
        let total = targets.len();
        // Eager, with owned inputs: a lazy `map` trips a higher-ranked-lifetime limit.
        let reads: Vec<_> = raw
            .iter()
            .map(|raw| one_line(&sanitize_url(raw), Some(TEXT_LIMIT)))
            .zip(targets)
            .map(|(shown, target)| {
                let ctx = ctx.clone();
                async move { read_update(&ctx, shown, target).await }
            })
            .collect();
        let (done, cancelled) = until_cancelled(reads, &session.cancel_token()).await;
        let mut failed: Vec<Failure> = Vec::new();
        for review in &done {
            failed.extend(review.failures.iter().cloned());
            notes.extend(review.notes.iter().cloned());
        }
        let mines: Vec<Option<Mine>> = done
            .iter()
            .map(|r| logins.as_ref().map(|l| mine_of(r, l)))
            .collect();
        if as_json {
            let rows: Vec<JsonRow<'_>> = done
                .iter()
                .zip(&mines)
                .map(|(review, mine)| JsonRow {
                    review,
                    mine: *mine,
                })
                .collect();
            let doc = serde_json::json!({"rows": rows, "failed": failed, "notes": notes});
            session.display.println(
                &serde_json::to_string_pretty(&doc).expect("serialising review rows is infallible"),
            );
        } else {
            for line in render_table(&done, &mines, &failed, &notes) {
                session.display.println(&line);
            }
        }
        if cancelled {
            let read = format!("{} of {total} updates", done.len());
            if failed.is_empty() {
                return Err(CommandError::Cancelled(format!("read {read}")));
            }
            // A failure collected before the cancel outranks it.
            return Err(CommandError::Other(format!(
                "{} lookup(s) failed, so the rows above are incomplete; cancelled after \
                 reading {read}",
                failed.len()
            )));
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "{} lookup(s) failed, so the rows above are incomplete",
                failed.len()
            )))
        }
    }
}

/// Run `reads` at most [`CONCURRENCY`] at a time until all finish or `cancel`
/// fires; returns the finished ones in input order and whether it was cancelled.
/// Unordered, so a cancel keeps every finished read, not only those ahead of
/// the slowest.
async fn until_cancelled<T, F: std::future::Future<Output = T>>(
    reads: Vec<F>,
    cancel: &tokio_util::sync::CancellationToken,
) -> (Vec<T>, bool) {
    let mut pending = stream::iter(
        reads
            .into_iter()
            .enumerate()
            .map(|(i, read)| async move { (i, read.await) }),
    )
    .buffer_unordered(CONCURRENCY);
    let mut done = Vec::new();
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            next = pending.next() => match next {
                Some(read) => done.push(read),
                None => break,
            },
            () = cancel.cancelled() => {
                cancelled = true;
                break;
            }
        }
    }
    done.sort_by_key(|(i, _)| *i);
    (done.into_iter().map(|(_, r)| r).collect(), cancelled)
}

/// One cell over the tester groups: `?` when the reviews could not be read,
/// `-` when there are none, the value for one, `group:value` for several.
fn per_group(review: &UpdateReview, cell: impl Fn(&GroupReview) -> String) -> String {
    match testers(review) {
        None => "?".to_owned(),
        Some(groups) if groups.is_empty() => "-".to_owned(),
        Some(groups) if groups.len() == 1 => cell(groups[0]),
        Some(groups) => groups
            .iter()
            .map(|g| format!("{}:{}", g.group, cell(g)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn since_cell(review: &UpdateReview) -> String {
    if !review.in_review() {
        return review.state_since.to_string();
    }
    match testers(review) {
        None => "?".to_owned(),
        Some(groups) if groups.is_empty() => "-".to_owned(),
        Some(groups) => {
            // RFC 3339-shaped times order as strings.
            let latest = groups
                .iter()
                .filter_map(|g| match &g.since {
                    Field::Known(t) => Some(t.as_str()),
                    _ => None,
                })
                .max();
            match latest {
                Some(t) => t.to_owned(),
                None if groups.iter().any(|g| g.since == Field::Unknown) => "?".to_owned(),
                None => "-".to_owned(),
            }
        }
    }
}

fn row_cells(review: &UpdateReview, mine: Option<Mine>) -> Vec<String> {
    let mut cols = vec![
        review.update.clone(),
        per_group(review, |g| g.owner.to_string()),
    ];
    if let Some(mine) = mine {
        cols.push(mine.to_string());
    }
    let gate = if review.in_review() {
        per_group(review, |g| g.gate.to_string())
    } else {
        review.state.to_string()
    };
    cols.extend([
        gate,
        since_cell(review),
        review.created.to_string(),
        review.head.to_string().chars().take(HEAD_WIDTH).collect(),
        review.base.to_string(),
        review.template.to_string(),
    ]);
    cols
}

fn render_table(
    reviews: &[UpdateReview],
    mines: &[Option<Mine>],
    failed: &[Failure],
    notes: &[String],
) -> Vec<String> {
    let with_mine = mines.iter().any(Option::is_some);
    let mut header = vec!["UPDATE", "OWNER"];
    if with_mine {
        header.push("MINE");
    }
    header.extend(["GATE", "SINCE", "CREATED", "HEAD", "BASE", "TEMPLATE"]);
    let table: Vec<Vec<String>> = reviews
        .iter()
        .zip(mines)
        .map(|(r, m)| row_cells(r, *m))
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            table
                .iter()
                .map(|cols| cols[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cols: Vec<&str>| {
        cols.iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    let mut out = vec![line(header)];
    out.extend(
        table
            .iter()
            .map(|cols| line(cols.iter().map(String::as_str).collect())),
    );
    if !failed.is_empty() {
        out.push("could not read:".to_owned());
        out.extend(
            failed
                .iter()
                .map(|f| format!("  {}: {}: {}", f.update, f.source, f.reason)),
        );
    }
    out.extend(notes.iter().map(|n| format!("note: {n}")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::teregen::document_json;
    use crate::commands::testkit::{Buffer, empty_session, matches, session_with_hosts};
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SLFO: &str = "SUSE:SLFO:1.2:7";
    const HEAD: &str = "0123456789abcdef0123";
    const PR: &str = "/api/v1/repos/products/SLFO/pulls/7";
    /// Nothing listens on the discard port.
    const DEAD: &str = "http://127.0.0.1:9";

    fn ts(day: u32) -> String {
        format!("2026-01-{day:02}T00:00:00+00:00")
    }

    fn point_at(session: &mut Session, server: &MockServer) {
        session.config.teregen_api = server.uri();
        session.config.teregen_api_v2 = format!("{}/v2", server.uri());
        session.config.gitea_url = server.uri();
        session.config.gitea_token = "tok".to_owned();
        session.config.obs_api_url = server.uri();
    }

    /// A session with a loaded template and a host, its sources at `server`.
    fn session_for(server: &MockServer) -> (Session, Buffer) {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:9:9", &["h1"], "ok");
        point_at(&mut session, server);
        (session, buf)
    }

    async fn get(server: &MockServer, at: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    async fn mount_status(server: &MockServer, rrid: &str, body: Value) {
        get(server, &format!("/reports/{rrid}/status"), ok(body)).await;
    }

    async fn mount_template(server: &MockServer, rrid: &str) {
        mount_status(
            server,
            rrid,
            json!({"template": true, "minion_state": "finished"}),
        )
        .await;
    }

    /// The published SLFO document, naming pull request `number` under
    /// `gitea` at `commit`.
    async fn mount_slfo_document_at(server: &MockServer, gitea: &str, number: u64, commit: &str) {
        let origin = format!(
            r#""origin": {{"api": "{gitea}/api/v1/repos/products/SLFO/pulls/{number}", "commit": "{commit}"}}"#
        );
        let doc = document_json(SLFO, "slfo").replace(r#""origin": {}"#, &origin);
        get(
            server,
            &format!("/v2/reports/{SLFO}"),
            ResponseTemplate::new(200).set_body_string(doc),
        )
        .await;
    }

    async fn mount_slfo_document(server: &MockServer, number: u64, commit: &str) {
        mount_slfo_document_at(server, &server.uri(), number, commit).await;
    }

    /// The pull request, its one page of `reviews` and its `comments`.
    async fn mount_pull(server: &MockServer, reviews: Value, comments: Value) {
        get(
            server,
            PR,
            ok(
                json!({"state": "open", "created_at": ts(1), "head": {"sha": HEAD},
                      "base": {"ref": "main"}}),
            ),
        )
        .await;
        for (page, body) in [("1", reviews), ("2", json!([]))] {
            Mock::given(method("GET"))
                .and(path(format!("{PR}/reviews")))
                .and(query_param("page", page))
                .respond_with(ok(body))
                .mount(server)
                .await;
        }
        get(
            server,
            "/api/v1/repos/products/SLFO/issues/7/comments",
            ok(comments),
        )
        .await;
    }

    fn owed_review() -> Value {
        json!([{"id": 3, "state": "REQUEST_REVIEW", "dismissed": false, "stale": false,
                "submitted_at": ts(3), "user": {"login": "qam-sle-review"}}])
    }

    fn assigned_to(user: &str) -> Value {
        json!([{"body": mtui_datasources::assign_marker(user, "qam-sle"), "updated_at": ts(2)}])
    }

    async fn run_for(session: &mut Session, argv: &[&str], budget: Duration) -> CommandResult {
        ReviewState
            .call_with_budget(session, &matches(&ReviewState, argv), budget)
            .await
    }

    async fn run(session: &mut Session, argv: &[&str]) -> CommandResult {
        run_for(session, argv, Duration::from_secs(10)).await
    }

    async fn requests(server: &MockServer) -> Vec<(String, String)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_owned()))
            .collect()
    }

    fn json_out(buf: &Buffer) -> Value {
        serde_json::from_str(&buf.contents()).expect("--json prints one JSON document")
    }

    fn first_review(out: &Value) -> &Value {
        &out["rows"][0]["reviews"]["value"][0]
    }

    /// The criterion's case: nothing loaded, and nothing is after the read.
    #[tokio::test]
    async fn an_slfo_row_reads_on_an_empty_session() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document(&server, 7, HEAD).await;
        mount_pull(&server, owed_review(), assigned_to("alice")).await;
        let (mut session, buf) = empty_session();
        point_at(&mut session, &server);

        run(&mut session, &[SLFO]).await.unwrap();

        insta::assert_snapshot!(buf.contents());
        assert!(session.templates.rrids().is_empty());
        assert!(session.targets().names().is_empty());
        let seen = requests(&server).await;
        assert!(seen.iter().all(|(m, _)| m == "GET"), "{seen:?}");
        assert_eq!(seen.len(), 6, "{seen:?}");
    }

    /// A loaded template and its hosts stay as they were; the JSON shape is
    /// pinned here.
    #[tokio::test]
    async fn a_loaded_session_is_left_alone_and_the_json_is_typed() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document(&server, 7, HEAD).await;
        mount_pull(&server, owed_review(), assigned_to("alice")).await;
        let (mut session, buf) = session_for(&server);
        let loaded = session.templates.rrids();
        let active = session.templates.active_rrid().map(str::to_owned);
        let hosts = session.targets().names();

        run(&mut session, &[SLFO, "--json"]).await.unwrap();

        insta::assert_snapshot!(buf.contents());
        assert_eq!(session.templates.rrids(), loaded);
        assert_eq!(session.templates.active_rrid().map(str::to_owned), active);
        assert_eq!(session.targets().names(), hosts);
        let seen = requests(&server).await;
        assert!(seen.iter().all(|(m, _)| m == "GET"), "{seen:?}");
    }

    #[tokio::test]
    async fn a_template_on_another_commit_is_stale() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document(&server, 7, "feedface").await;
        mount_pull(&server, owed_review(), json!([])).await;
        let (mut session, buf) = session_for(&server);
        run(&mut session, &[SLFO, "--json"]).await.unwrap();
        let out = json_out(&buf);
        assert_eq!(out["rows"][0]["template"], json!({"status": "stale"}));
        assert_eq!(first_review(&out)["owner"], json!({"status": "none"}));
        assert_eq!(
            out["rows"][0]["head"],
            json!({"status": "known", "value": HEAD})
        );
        assert_eq!(out["failed"], json!([]));
    }

    #[tokio::test]
    async fn a_pull_request_url_reads_the_review_and_asks_no_template() {
        let server = MockServer::start().await;
        mount_pull(&server, owed_review(), assigned_to("alice")).await;
        let (mut session, buf) = session_for(&server);
        let url = format!("{}/products/SLFO/pulls/7", server.uri());
        run(&mut session, &[&url, "--json"]).await.unwrap();
        let out = json_out(&buf);
        assert_eq!(
            first_review(&out)["owner"],
            json!({"status": "known", "login": "alice"})
        );
        assert_eq!(first_review(&out)["gate"]["state"], "owed");
        assert_eq!(
            out["rows"][0]["template"],
            json!({"status": "not_applicable"})
        );
        let seen = requests(&server).await;
        assert!(
            seen.iter().all(|(_, p)| !p.starts_with("/reports")),
            "{seen:?}"
        );
    }

    /// A pull request on another host is refused before anything is sent to
    /// it, so the token never travels there.
    #[tokio::test]
    async fn a_foreign_pull_request_is_refused_without_sending_the_token() {
        let trusted = MockServer::start().await;
        let foreign = MockServer::start().await;
        mount_pull(&foreign, owed_review(), json!([])).await;
        let (mut session, buf) = session_for(&trusted);
        let url = format!("{}/products/SLFO/pulls/7", foreign.uri());
        run(&mut session, &[&url, "--json"]).await.unwrap_err();
        let out = json_out(&buf);
        assert_eq!(out["failed"][0]["source"], "gitea");
        assert!(
            out["failed"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("untrusted"),
            "{out}"
        );
        let received = foreign.received_requests().await.unwrap();
        assert!(
            received
                .iter()
                .all(|r| !r.headers.contains_key("authorization")),
            "{received:?}"
        );
        assert!(received.is_empty(), "{received:?}");
    }

    /// Every source fails differently; each failure is named, its cells are
    /// `?`, the rest still render, and the command fails.
    #[tokio::test]
    async fn each_failed_source_is_named_and_fails_the_command() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document(&server, 7, HEAD).await;
        get(&server, PR, ok(json!({"head": {"sha": HEAD}}))).await;
        get(
            &server,
            &format!("{PR}/reviews"),
            ResponseTemplate::new(503),
        )
        .await;
        get(
            &server,
            "/api/v1/repos/products/SLFO/issues/7/comments",
            ok(json!([])),
        )
        .await;
        mount_status(
            &server,
            "SUSE:SLFO:1.2:8",
            json!({"minion_state": "finished"}),
        )
        .await;
        get(
            &server,
            "/v2/reports/SUSE:SLFO:1.2:8",
            ResponseTemplate::new(404),
        )
        .await;
        let (mut session, buf) = session_for(&server);
        let err = run(&mut session, &[SLFO, "S:S:1.2:8"]).await.unwrap_err();
        insta::assert_snapshot!(buf.contents().replace(&server.uri(), "<server>"));
        assert!(
            matches!(&err, CommandError::Other(m) if m.starts_with("3 lookup(s) failed")),
            "{err:?}"
        );
    }

    async fn template_failure(teregen: &str) -> Value {
        let server = MockServer::start().await;
        mount_slfo_document(&server, 7, HEAD).await;
        mount_pull(&server, owed_review(), json!([])).await;
        get(
            &server,
            &format!("/reports/{SLFO}/status"),
            ResponseTemplate::new(502),
        )
        .await;
        let (mut session, buf) = session_for(&server);
        if !teregen.is_empty() {
            session.config.teregen_api = teregen.to_owned();
        }
        run(&mut session, &[SLFO, "--json"]).await.unwrap_err();
        json_out(&buf)
    }

    #[tokio::test]
    async fn an_unreachable_or_failing_teregen_is_unknown_never_missing() {
        for (case, teregen) in [("unreachable", DEAD), ("5xx", "")] {
            let out = template_failure(teregen).await;
            assert_eq!(
                out["rows"][0]["template"],
                json!({"status": "unknown"}),
                "{case}"
            );
            assert_eq!(first_review(&out)["gate"]["state"], "owed", "{case}");
            assert_eq!(out["failed"][0]["source"], "teregen", "{case}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_gitea_is_named() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document_at(&server, DEAD, 7, HEAD).await;
        let (mut session, buf) = session_for(&server);
        session.config.gitea_url = DEAD.to_owned();
        run(&mut session, &[SLFO, "--json"]).await.unwrap_err();
        let out = json_out(&buf);
        assert_eq!(out["failed"][0]["source"], "gitea");
        assert_eq!(out["rows"][0]["reviews"], json!({"status": "unknown"}));
        assert_eq!(out["rows"][0]["template"], json!({"status": "unknown"}));
    }

    #[tokio::test]
    async fn a_document_naming_another_pull_request_is_a_failure() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        mount_slfo_document(&server, 70, HEAD).await;
        let (mut session, buf) = session_for(&server);
        run(&mut session, &[SLFO, "--json"]).await.unwrap_err();
        let out = json_out(&buf);
        assert_eq!(
            out["failed"][0]["reason"],
            "the published document names pull request 70, not the RRID's 7"
        );
        assert_eq!(out["rows"][0]["reviews"], json!({"status": "unknown"}));
        let seen = requests(&server).await;
        assert!(
            seen.iter().all(|(_, p)| !p.starts_with("/api/v1")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_slow_source_times_out_alone() {
        let server = MockServer::start().await;
        get(
            &server,
            &format!("/reports/{SLFO}/status"),
            ok(json!({"template": true})).set_delay(Duration::from_secs(5)),
        )
        .await;
        mount_slfo_document(&server, 7, HEAD).await;
        mount_pull(&server, owed_review(), json!([])).await;
        let (mut session, buf) = session_for(&server);
        run_for(&mut session, &[SLFO, "--json"], Duration::from_millis(300))
            .await
            .unwrap_err();
        let out = json_out(&buf);
        assert_eq!(out["failed"][0]["reason"], "no answer within 300ms");
        assert_eq!(first_review(&out)["gate"]["state"], "owed");
    }

    /// Answers after `delay`, cancelling `token` as the request arrives: a
    /// cancel caused by the read, not by a timer.
    struct CancelOnRequest {
        token: tokio_util::sync::CancellationToken,
        response: ResponseTemplate,
    }

    impl wiremock::Respond for CancelOnRequest {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            self.token.cancel();
            self.response.clone()
        }
    }

    /// Mounts an SLFO status read that cancels the session when it arrives
    /// and never answers in time.
    async fn mount_cancelling_status(server: &MockServer, session: &mut Session) {
        let token = tokio_util::sync::CancellationToken::new();
        session.set_cancel_token(token.clone());
        Mock::given(method("GET"))
            .and(path(format!("/reports/{SLFO}/status")))
            .respond_with(CancelOnRequest {
                token,
                response: ok(json!({"template": true})).set_delay(Duration::from_secs(30)),
            })
            .mount(server)
            .await;
    }

    /// A cancel during a read fails `Cancelled`, counting what was read.
    #[tokio::test]
    async fn a_cancel_stops_the_reads_and_counts_them() {
        let server = MockServer::start().await;
        let (mut session, _buf) = session_for(&server);
        mount_cancelling_status(&server, &mut session).await;
        let started = std::time::Instant::now();
        let err = run(&mut session, &[SLFO]).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Cancelled(m) if m == "read 0 of 1 updates"),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A read that finished behind a slower one is kept on a cancel, in its
    /// place: the slow read cancels as it starts, the fast one is ready.
    #[tokio::test]
    async fn a_cancel_keeps_every_finished_read_in_order() {
        let token = tokio_util::sync::CancellationToken::new();
        let slow = {
            let token = token.clone();
            async move {
                token.cancel();
                std::future::pending::<&str>().await
            }
        };
        let reads: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = &str> + Send>>> = vec![
            Box::pin(slow),
            Box::pin(async { "fast" }),
            Box::pin(async { "also" }),
        ];
        let (done, cancelled) = until_cancelled(reads, &token).await;
        assert!(cancelled);
        assert_eq!(done, ["fast", "also"]);
    }

    /// A lookup failure collected before a cancel outranks it: the error
    /// names both, so a client reading the error kind sees the failure.
    #[tokio::test]
    async fn a_failure_before_a_cancel_outranks_it() {
        let trusted = MockServer::start().await;
        let foreign = MockServer::start().await;
        let (mut session, _buf) = session_for(&trusted);
        mount_cancelling_status(&trusted, &mut session).await;
        let url = format!("{}/products/SLFO/pulls/7", foreign.uri());
        let err = run(&mut session, &[SLFO, &url]).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m == "1 lookup(s) failed, so the rows \
                above are incomplete; cancelled after reading 1 of 2 updates"),
            "{err:?}"
        );
    }

    /// TeReGen flagging its document stale is a `document` failure naming no
    /// pull request, with the template `stale`, and the call fails.
    #[tokio::test]
    async fn a_stale_document_fails_the_row_and_reads_stale() {
        let server = MockServer::start().await;
        mount_template(&server, SLFO).await;
        get(
            &server,
            &format!("/v2/reports/{SLFO}"),
            ResponseTemplate::new(409),
        )
        .await;
        let (mut session, buf) = session_for(&server);
        let err = run(&mut session, &[SLFO, "--json"]).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)), "{err:?}");
        let out = json_out(&buf);
        assert_eq!(out["failed"][0]["source"], "document");
        assert_eq!(out["rows"][0]["template"], json!({"status": "stale"}));
    }

    fn group_at(group: &str, since: &str) -> GroupReview {
        GroupReview {
            group: group.to_owned(),
            automated: false,
            owner: Owner::None,
            gate: crate::review::Gate::None,
            since: Field::Known(since.to_owned()),
        }
    }

    /// Several groups: SINCE is the latest of their known times.
    #[test]
    fn since_is_the_latest_group_time() {
        let review = UpdateReview {
            update: "U".to_owned(),
            workflow: Field::Unknown,
            state: Field::Known("review".to_owned()),
            state_since: Field::Unknown,
            reviews: Field::Known(vec![
                group_at("qam-a", "2026-09-02T07:25:47"),
                group_at("qam-b", "2026-09-01T00:33:57"),
            ]),
            created: Field::Unknown,
            head: Field::NotApplicable,
            base: Field::NotApplicable,
            template: crate::review::Template::Unknown,
            failures: Vec::new(),
            notes: Vec::new(),
        };
        assert_eq!(since_cell(&review), "2026-09-02T07:25:47");
    }

    #[tokio::test]
    async fn a_bad_argument_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let (mut session, _buf) = session_for(&server);
        let err = run(&mut session, &[SLFO, "not-an-update"])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.starts_with("not-an-update: neither")),
            "{err:?}"
        );
        assert!(requests(&server).await.is_empty());
    }

    /// An oscrc for `api` whose key is never read: no 401 is served.
    fn oscrc_for(api: &str, user: &str, dir: &tempfile::TempDir) -> std::path::PathBuf {
        let key = dir.path().join("id_unused");
        std::fs::write(&key, "never read\n").unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            format!("[{api}]\nuser = {user}\nsshkey = {}\n", key.display()),
        )
        .unwrap();
        oscrc
    }

    const T1: &str = "2026-09-01T00:33:57";
    const T2: &str = "2026-09-02T07:25:47";

    fn obs_request(id: u64, project: &str, state: &str, reviews: &str) -> String {
        format!(
            "<request id='{id}'><action type='maintenance_release'>\
             <source project='{project}' package='p'/></action>\
             <state name='{state}' when='{T2}' created='{T1}'/>{reviews}</request>"
        )
    }

    fn held_by_alice() -> String {
        format!(
            "<review state='accepted' by_group='qam-sle'><history who='alice' when='{T1}'>\
             <description>Review got accepted</description></history></review>\
             <review state='new' when='{T2}' who='alice' by_user='alice'>\
             <history who='alice' when='{T1}'><description>Review got assigned</description>\
             </history><comment>reassigned review for group qam-sle to user alice</comment></review>"
        )
    }

    async fn mount_obs(server: &MockServer, id: u64, response: ResponseTemplate) {
        get(server, &format!("/request/{id}"), response).await;
    }

    /// Runs `argv` with `$OSC_CONFIG` at an oscrc for `api` naming `user`.
    #[allow(unsafe_code)]
    async fn run_with_oscrc(
        session: &mut Session,
        api: &str,
        user: &str,
        argv: &[&str],
    ) -> CommandResult {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = oscrc_for(api, user, &dir);
        // SAFETY: callers hold `#[serial(osc_config_env)]`.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        let result = run(session, argv).await;
        // SAFETY: as above.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        result
    }

    /// Two qam groups (one open, one held after an assign) plus an
    /// automation group, a request that left review, and one whose source
    /// project is not the RRID's incident.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn obs_rows_read_each_qam_group_and_refuse_a_foreign_project() {
        let server = MockServer::start().await;
        let reviews = held_by_alice()
            + "<review state='new' by_group='qam-cloud'/>"
            + "<review state='new' by_group='qam-openqa'/>";
        for (id, body) in [
            (1, obs_request(1, "SUSE:Maintenance:10", "review", &reviews)),
            (2, obs_request(2, "SUSE:Maintenance:20", "accepted", "")),
            (3, obs_request(3, "SUSE:Maintenance:99", "review", "")),
        ] {
            mount_obs(
                &server,
                id,
                ResponseTemplate::new(200).set_body_string(body),
            )
            .await;
        }
        for rrid in [
            "SUSE:Maintenance:10:1",
            "SUSE:Maintenance:20:2",
            "SUSE:Maintenance:30:3",
        ] {
            mount_template(&server, rrid).await;
        }
        let (mut session, buf) = session_for(&server);
        run_with_oscrc(
            &mut session,
            &server.uri(),
            "qamuser",
            &["S:M:10:1", "SUSE:Maintenance:20:2", "SUSE:Maintenance:30:3"],
        )
        .await
        .unwrap_err();
        insta::assert_snapshot!(buf.contents());
        let seen = requests(&server).await;
        assert!(seen.iter().all(|(m, _)| m == "GET"), "{seen:?}");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn an_unreachable_or_failing_obs_is_named() {
        for case in ["unreachable", "5xx"] {
            let server = MockServer::start().await;
            mount_template(&server, "SUSE:Maintenance:10:1").await;
            mount_obs(&server, 1, ResponseTemplate::new(503)).await;
            let (mut session, buf) = session_for(&server);
            let api = if case == "unreachable" {
                DEAD.to_owned()
            } else {
                server.uri()
            };
            session.config.obs_api_url = api.clone();
            run_with_oscrc(&mut session, &api, "qamuser", &["S:M:10:1", "--json"])
                .await
                .unwrap_err();
            let out = json_out(&buf);
            assert_eq!(out["failed"][0]["source"], "obs", "{case}");
            assert_eq!(
                out["rows"][0]["reviews"],
                json!({"status": "unknown"}),
                "{case}"
            );
            assert_eq!(
                out["rows"][0]["template"],
                json!({"status": "current"}),
                "{case}: TeReGen still answers"
            );
        }
    }

    /// An accepted event without a `when`: SINCE is `?` in the table and
    /// unknown in `--json`, never a known empty value.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn an_accepted_event_without_a_time_is_an_unknown_since() {
        let server = MockServer::start().await;
        let reviews = "<review state='accepted' by_group='qam-sle'><history who='alice'>\
                       <description>Review got accepted</description></history></review>";
        mount_obs(
            &server,
            1,
            ResponseTemplate::new(200).set_body_string(obs_request(
                1,
                "SUSE:Maintenance:10",
                "review",
                reviews,
            )),
        )
        .await;
        mount_template(&server, "SUSE:Maintenance:10:1").await;
        let (mut session, buf) = session_for(&server);
        run_with_oscrc(
            &mut session,
            &server.uri(),
            "qamuser",
            &["S:M:10:1", "--json"],
        )
        .await
        .unwrap();
        let out = json_out(&buf);
        assert_eq!(first_review(&out)["since"], json!({"status": "unknown"}));
        let (mut session, buf) = session_for(&server);
        run_with_oscrc(&mut session, &server.uri(), "qamuser", &["S:M:10:1"])
            .await
            .unwrap();
        let table = buf.contents();
        let row = table.lines().nth(1).expect("one row");
        let header = table.lines().next().unwrap();
        let since_at = header.find("SINCE").unwrap();
        assert_eq!(
            row[since_at..].split_whitespace().next(),
            Some("?"),
            "{table}"
        );
    }

    /// `--mine` names a held update `yes`, another's `no` and a disputed one
    /// `?`, never `no`.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    async fn mine_reads_each_owner_against_the_logins() {
        let server = MockServer::start().await;
        get(&server, "/api/v1/user", ok(json!({"login": "alice"}))).await;
        mount_pull(&server, owed_review(), assigned_to("alice")).await;
        let disputed = format!(
            "<review state='accepted' by_group='qam-sle'><history who='bob' when='{T1}'>\
             <description>Review got accepted</description></history></review>"
        );
        let held = held_by_alice().replace("alice", "carol");
        for (id, project, reviews) in [
            (1, "SUSE:Maintenance:10", disputed),
            (2, "SUSE:Maintenance:20", held),
        ] {
            mount_obs(
                &server,
                id,
                ResponseTemplate::new(200)
                    .set_body_string(obs_request(id, project, "review", &reviews)),
            )
            .await;
        }
        for rrid in ["SUSE:Maintenance:10:1", "SUSE:Maintenance:20:2"] {
            mount_template(&server, rrid).await;
        }
        let (mut session, buf) = session_for(&server);
        let url = format!("{}/products/SLFO/pulls/7", server.uri());
        run_with_oscrc(
            &mut session,
            &server.uri(),
            "qamuser",
            &[&url, "S:M:10:1", "S:M:20:2", "--mine", "--json"],
        )
        .await
        .unwrap();
        let out = json_out(&buf);
        let mines: Vec<&Value> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["mine"])
            .collect();
        assert_eq!(mines, [&json!("yes"), &json!("unknown"), &json!("no")]);
    }

    #[test]
    fn mine_wording_is_this_commands() {
        use super::super::updates::mine_logins;
        let mine = mine_logins(
            "https://api.example",
            Err("gone".to_owned()),
            Some(Ok("alice".to_owned())),
            &REVIEW_STATE_MINE,
        )
        .unwrap();
        let note = mine.note.unwrap();
        assert!(note.ends_with("; only alice counts as mine"), "{note}");
        for (gitea, ending) in [
            (
                None,
                "; fix the oscrc or set a [gitea] token, or drop --mine",
            ),
            (
                Some(Err(mtui_datasources::GiteaError::MissingToken)),
                "; fix the oscrc or the [gitea] token, or drop --mine",
            ),
        ] {
            let Err(CommandError::Other(message)) = mine_logins(
                "https://api.example",
                Err("gone".to_owned()),
                gitea,
                &REVIEW_STATE_MINE,
            ) else {
                panic!("no login must refuse");
            };
            assert!(message.ends_with(ending), "{message}");
        }
    }
}
