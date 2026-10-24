//! The `openqa_jobs` command — list the individual openQA jobs for an update.

use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::oqa_search as oqa;

use super::row_budget::{crush, row_notice};
use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// openQA results that count as "not a failure" for the `--failed` filter.
const PASSING: &[&str] = &["passed", "softfailed"];
/// openQA results that are neutral (neither pass nor fail).
const NEUTRAL: &[&str] = &["obsoleted", "skipped"];
/// The finished job states; in any other (`scheduled`, `running`, ...) the
/// `result` is not yet meaningful.
const TERMINAL_STATES: &[&str] = &["done", "cancelled"];

/// Whether a job has not finished. `--failed` must not count these as failures;
/// they are surfaced separately as "N still pending".
fn is_pending(job: &oqa::JobResult) -> bool {
    !TERMINAL_STATES.contains(&job.state.as_str())
}

/// Narrowing flags named in the truncation notice.
const JOBS_HINT: &str = "--failed/--arch/--test/--flavor/--version/--limit/--offset";

/// Row-budget severity: failed jobs (`failed`, `incomplete`,
/// `timeout_exceeded`, ...) outrank pending ones, which outrank passing and
/// neutral jobs, so the actionable rows survive a truncation.
fn anomaly_severity(job: &oqa::JobResult) -> u8 {
    if is_pending(job) {
        1
    } else if PASSING.contains(&job.result.as_str()) || NEUTRAL.contains(&job.result.as_str()) {
        0
    } else {
        2
    }
}

/// Compiles a shell-style glob (`*` and `?`) into an anchored regex.
fn glob_regex(glob: &str) -> Result<regex::Regex, regex::Error> {
    let pattern = regex::escape(glob).replace(r"\*", ".*").replace(r"\?", ".");
    regex::Regex::new(&format!("^{pattern}$"))
}

/// The `VERSION FLAVOR MACHINE` settings of a job, skipping empty ones.
fn job_settings(job: &oqa::JobResult) -> String {
    [&job.version, &job.flavor, &job.machine]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The lines telling what the default listing leaves out: earlier attempts of
/// the listed scenarios, and jobs past the read limit.
fn fetch_notes(superseded: usize, complete: bool) -> Vec<String> {
    let mut notes = Vec::new();
    if superseded > 0 {
        let attempts = if superseded == 1 {
            "attempt"
        } else {
            "attempts"
        };
        notes.push(format!(
            "{superseded} earlier {attempts} not listed (--all lists every attempt)"
        ));
    }
    if !complete {
        notes.push(format!(
            "…[not every job of this build could be read (at most the newest {}); older jobs may be missing]",
            oqa::MAX_INCIDENT_JOBS
        ));
    }
    notes
}

/// Lists the individual openQA jobs for the loaded update's incident build.
///
/// It lists the newest non-obsoleted job of each scenario by default, as
/// openQA's `latest=1` does; `--all` lists every attempt. `--failed`
/// shows only genuinely failed jobs (pending/unfinished jobs are excluded and
/// reported as "N still pending"), and `--arch`, `--test` (glob), `--flavor`
/// and `--version` narrow by job settings. The listing is paged with
/// `--offset`/`--limit` and capped by the shared row budget, keeping failed,
/// then pending jobs ahead of passing ones. Requires a loaded update.
pub struct OpenQAJobs;

#[async_trait]
impl Command for OpenQAJobs {
    fn name(&self) -> &'static str {
        "openqa_jobs"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the individual openQA jobs for the loaded update's incident build.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help(
                    "list every attempt: earlier runs of a retriggered scenario and \
                     obsoleted jobs too (default: the newest non-obsoleted job of each scenario)",
                ),
        )
        .arg(
            Arg::new("failed")
                .long("failed")
                .action(ArgAction::SetTrue)
                .help("show only genuinely failed jobs (pending/unfinished excluded)"),
        )
        .arg(
            Arg::new("arch")
                .long("arch")
                .value_name("ARCH")
                .help("only jobs for this architecture"),
        )
        .arg(
            Arg::new("test")
                .long("test")
                .value_name("GLOB")
                .help("only jobs whose test name matches this glob (* and ?), e.g. '*incidentinstall*'"),
        )
        .arg(
            Arg::new("flavor")
                .long("flavor")
                .value_name("FLAVOR")
                .help("only jobs with this FLAVOR setting"),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .value_name("VERSION")
                .help("only jobs with this VERSION setting, e.g. 15-SP6"),
        )
        .arg(
            Arg::new("limit")
                .long("limit")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .default_value("0")
                .help("cap the number of rows after --offset (0 = all)"),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("N")
                .value_parser(clap::value_parser!(usize))
                .default_value("0")
                .help("skip the first N rows (0 = from the start); with --limit, page any middle slice"),
        )
        .arg(
            Arg::new("url_openqa")
                .long("url-openqa")
                .value_name("URL")
                .help("Override openQA URL (default: config openqa_instance)"),
        )
        .arg(
            Arg::new("url_dashboard_qam")
                .long("url-dashboard-qam")
                .value_name("URL")
                .help("Override QAM Dashboard base URL (default: derived from config)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = [
            "--all",
            "--failed",
            "--arch",
            "--test",
            "--flavor",
            "--version",
            "--limit",
            "--offset",
            "--url-openqa",
            "--url-dashboard-qam",
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

        let every_attempt = args.get_flag("all");
        let only_failed = args.get_flag("failed");
        let arch_filter = args.get_one::<String>("arch").cloned();
        let test_filter = args
            .get_one::<String>("test")
            .map(|glob| {
                glob_regex(glob)
                    .map_err(|e| CommandError::Other(format!("invalid --test glob {glob:?}: {e}")))
            })
            .transpose()?;
        let flavor_filter = args.get_one::<String>("flavor").cloned();
        let version_filter = args.get_one::<String>("version").cloned();
        let limit = args.get_one::<usize>("limit").copied().unwrap_or(0);
        let offset = args.get_one::<usize>("offset").copied().unwrap_or(0);

        let url_openqa = args
            .get_one::<String>("url_openqa")
            .cloned()
            .unwrap_or_else(|| session.config.openqa_instance.clone());
        let url_dashboard_qam = args
            .get_one::<String>("url_dashboard_qam")
            .cloned()
            .unwrap_or_else(|| {
                session
                    .config
                    .qem_dashboard_api
                    .trim_end_matches('/')
                    .strip_suffix("/api")
                    .unwrap_or(session.config.qem_dashboard_api.trim_end_matches('/'))
                    .to_owned()
            });

        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;
        let openqa_transport = session
            .openqa_transport()
            .map_err(|e| CommandError::Other(format!("could not build openQA transport: {e}")))?;
        let oqa_client = mtui_datasources::openqa::build_openqa_client_with_transport(
            openqa_transport,
            &url_openqa,
        )
        .map_err(|e| CommandError::Other(format!("could not build openQA client: {e}")))?;

        // The incident id is the maintenance id, but SLFO's is not an integer
        // ("1.2"), so fall back to the review id there. As `openqa_overview`.
        let effective_incident_id = if rrid.maintenance_id.parse::<i64>().is_ok() {
            rrid.maintenance_id.clone()
        } else {
            rrid.review_id.to_string()
        };

        let (build, _versions) =
            match oqa::get_incident_info(&http, &url_dashboard_qam, &effective_incident_id).await {
                Ok(v) => v,
                Err(e) => {
                    return Err(CommandError::Other(format!(
                        "QEM Dashboard query failed: {e}"
                    )));
                }
            };

        let fetched = match oqa::incident_jobs(&oqa_client, &build, every_attempt).await {
            Ok(j) => j,
            Err(e) => {
                return Err(CommandError::Other(format!(
                    "openQA jobs query failed: {e}"
                )));
            }
        };
        let matches = |j: &oqa::JobResult| {
            arch_filter.as_ref().is_none_or(|a| &j.arch == a)
                && test_filter.as_ref().is_none_or(|t| t.is_match(&j.test))
                && flavor_filter.as_ref().is_none_or(|f| &j.flavor == f)
                && version_filter.as_ref().is_none_or(|v| &j.version == v)
        };
        let is_failed = |j: &oqa::JobResult| {
            !is_pending(j)
                && !PASSING.contains(&j.result.as_str())
                && !NEUTRAL.contains(&j.result.as_str())
        };
        let mut jobs = fetched.jobs;
        jobs.retain(|j| matches(j));
        // Counted before `--failed` filters them out, so the message reflects
        // reality regardless of the active filter.
        let pending_count = jobs.iter().filter(|j| is_pending(j)).count();
        if only_failed {
            jobs.retain(|j| is_failed(j));
        }
        // Earlier attempts the same filters would have listed.
        let superseded = fetched
            .superseded
            .iter()
            .filter(|j| matches(j) && (!only_failed || is_failed(j)))
            .count();
        let fetch_notes = fetch_notes(superseded, fetched.complete);

        if jobs.is_empty() {
            let msg = session
                .display
                .yellow(&format!("No openQA jobs for build {build:?}"));
            session.display.println(&msg);
            if pending_count > 0 {
                let pending = session
                    .display
                    .yellow(&format!("{pending_count} still pending"));
                session.display.println(&pending);
            }
            for note in &fetch_notes {
                session.display.println(note);
            }
            return Ok(());
        }

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for j in &jobs {
            *counts.entry(j.result.clone()).or_default() += 1;
        }
        let summary = counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        session.display.println(&format!(
            "openQA jobs for build {build} ({}): {summary}",
            jobs.len()
        ));
        if pending_count > 0 {
            let pending = session
                .display
                .yellow(&format!("{pending_count} still pending"));
            session.display.println(&pending);
        }
        for note in &fetch_notes {
            session.display.println(note);
        }
        session.display.println("");

        // Header counts above cover the full filtered set; only the listing
        // is windowed (--offset/--limit) and then crushed to the row budget,
        // as in `list_refhosts`.
        let windowed: Vec<oqa::JobResult> = {
            let mut v: Vec<oqa::JobResult> = jobs.into_iter().skip(offset).collect();
            if limit > 0 && limit < v.len() {
                v.truncate(limit);
            }
            v
        };
        let crushed = crush(windowed, |j| j.url.clone(), anomaly_severity);
        for j in &crushed.kept {
            let result = if is_pending(j) {
                session.display.yellow(&format!("{:<15}", j.result))
            } else if PASSING.contains(&j.result.as_str()) {
                session.display.green(&format!("{:<15}", j.result))
            } else if NEUTRAL.contains(&j.result.as_str()) {
                session.display.yellow(&format!("{:<15}", j.result))
            } else {
                session.display.red(&format!("{:<15}", j.result))
            };
            let settings = job_settings(j);
            if settings.is_empty() {
                session
                    .display
                    .println(&format!("  {result} {:<8} {}  {}", j.arch, j.test, j.url));
            } else {
                session.display.println(&format!(
                    "  {result} {:<8} {}  [{settings}]  {}",
                    j.arch, j.test, j.url
                ));
            }
        }
        if crushed.truncated > 0 {
            session.display.println(&row_notice(
                crushed.truncated,
                crushed.total,
                crushed.anomaly_kept,
                crushed.anomaly_total,
                JOBS_HINT,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use wiremock::matchers::{
        header, header_exists, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(OpenQAJobs.name(), "openqa_jobs");
        assert_eq!(OpenQAJobs.scope(), Scope::Fanout);
    }

    #[test]
    fn completion_offers_flags_and_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = OpenQAJobs.complete(&session, "--f", "");
        assert_eq!(out, vec!["--failed", "--flavor"]);
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&OpenQAJobs, &[]);
        let err = OpenQAJobs.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// Mounts a dashboard build lookup + an openQA jobs list on one server.
    async fn server_with_jobs(jobs: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"settings": {"BUILD": "BUILD-42", "DISTRI": "sle", "VERSION": "15-SP5"}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs"))
            .and(query_param("build", "BUILD-42"))
            .and(query_param_is_missing("before"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jobs))
            .mount(&server)
            .await;
        // Every job is on the first page; the next one is empty.
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs"))
            .and(|req: &wiremock::Request| req.url.query_pairs().any(|(k, _)| k == "before"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "jobs": [] })),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn lists_jobs_with_summary() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "install", "arch": "x86_64", "result": "passed", "state": "done", "clone_id": null},
                {"id": 2, "test": "boot", "arch": "x86_64", "result": "failed", "state": "done", "clone_id": null},
            ]
        }))
        .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        OpenQAJobs.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("openQA jobs for build BUILD-42 (2)"), "{out}");
        assert!(out.contains("failed=1"), "{out}");
        assert!(out.contains("passed=1"), "{out}");
        assert!(out.contains("install"), "{out}");
        assert!(!out.contains("still pending"), "{out}");
        // Jobs without settings print no empty `[]`.
        assert!(!out.contains("[]"), "{out}");
    }

    #[tokio::test]
    async fn failed_filter_drops_passing_jobs() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "install", "arch": "x86_64", "result": "passed", "state": "done", "clone_id": null},
                {"id": 2, "test": "boot", "arch": "aarch64", "result": "failed", "state": "done", "clone_id": null},
            ]
        }))
        .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--failed",
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        OpenQAJobs.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("failed=1"), "{out}");
        assert!(
            !out.contains("passed="),
            "passing job should be filtered: {out}"
        );
        assert!(!out.contains("install"), "{out}");
    }

    /// `--failed` must not list unfinished jobs (`result:none`, non-terminal
    /// `state`) as failures, and must report them as pending.
    #[tokio::test]
    async fn failed_filter_excludes_pending_and_reports_count() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "install", "arch": "x86_64", "result": "passed", "state": "done", "clone_id": null},
                {"id": 2, "test": "boot", "arch": "x86_64", "result": "failed", "state": "done", "clone_id": null},
                {"id": 3, "test": "kdump", "arch": "x86_64", "result": "none", "state": "scheduled", "clone_id": null},
            ]
        }))
        .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--failed",
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        OpenQAJobs.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("boot"), "{out}");
        assert!(out.contains("failed=1"), "{out}");
        assert!(
            !out.contains("kdump"),
            "pending job must not be listed as failed: {out}"
        );
        assert!(out.contains("1 still pending"), "{out}");
    }

    /// A 5xx on the dashboard build lookup surfaces as Err, not an empty Ok the
    /// MCP surface would render as a bare success.
    #[tokio::test]
    async fn dashboard_failure_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        let err = OpenQAJobs.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// The same for a 5xx on the openQA jobs query itself.
    #[tokio::test]
    async fn openqa_jobs_failure_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"settings": {"BUILD": "BUILD-42", "DISTRI": "sle", "VERSION": "15-SP5"}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        let err = OpenQAJobs.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// In the default listing a pending job is still shown and counted. Color is
    /// stripped under `ColorMode::Never`, so the yellow-vs-red branch runs for
    /// coverage but is not asserted on.
    #[tokio::test]
    async fn pending_job_listed_and_counted_in_default_listing() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "kdump", "arch": "x86_64", "result": "none", "state": "running", "clone_id": null},
            ]
        }))
        .await;

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        OpenQAJobs.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("kdump"), "{out}");
        assert!(out.contains("1 still pending"), "{out}");
    }

    /// One openQA job as the jobs API returns it, with settings.
    fn job(
        id: i64,
        test: &str,
        result: &str,
        state: &str,
        settings: [&str; 3],
    ) -> serde_json::Value {
        let [flavor, version, machine] = settings;
        serde_json::json!({
            "id": id, "test": test, "result": result, "state": state, "clone_id": null,
            "settings": {"ARCH": "x86_64", "FLAVOR": flavor, "VERSION": version, "MACHINE": machine}
        })
    }

    /// Runs `openqa_jobs` with `extra` flags against `jobs`; returns stdout.
    async fn run_with(jobs: Vec<serde_json::Value>, extra: &[&str]) -> String {
        let server = server_with_jobs(serde_json::json!({ "jobs": jobs })).await;
        let uri = server.uri();
        let mut argv: Vec<&str> = extra.to_vec();
        argv.extend(["--url-dashboard-qam", &uri, "--url-openqa", &uri]);
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&OpenQAJobs, &argv);
        OpenQAJobs.call(&mut session, &args).await.unwrap();
        buf.contents()
    }

    /// Listed job rows (each carries its `/t<id>` URL).
    fn row_lines(out: &str) -> Vec<&str> {
        out.lines().filter(|l| l.contains("/t")).collect()
    }

    const DVD: [&str; 3] = ["Server-DVD-Incidents", "15-SP6", "64bit"];
    const MICRO: [&str; 3] = ["Micro-Incidents", "6.1", "64bit-2G"];

    #[test]
    fn glob_regex_matches_whole_name_with_wildcards() {
        let re = glob_regex("*incidentins?all*").unwrap();
        assert!(re.is_match("qam-incidentinstall"));
        assert!(re.is_match("incidentinstall-ha"));
        assert!(!re.is_match("install"));
        // Anchored: a pattern without wildcards matches the whole name only.
        assert!(
            !glob_regex("incidentinstall")
                .unwrap()
                .is_match("qam-incidentinstall")
        );
        assert!(
            !glob_regex("qam-*")
                .unwrap()
                .is_match("xqam-incidentinstall")
        );
        // Regex metacharacters in the glob are literal.
        let dot = glob_regex("a.b").unwrap();
        assert!(dot.is_match("a.b"));
        assert!(!dot.is_match("axb"));
    }

    #[test]
    fn severity_ranks_failed_over_pending_over_passing() {
        let mk = |result: &str, state: &str| {
            let mut job = oqa::JobResult::default();
            result.clone_into(&mut job.result);
            state.clone_into(&mut job.state);
            job
        };
        for failed in [
            "failed",
            "incomplete",
            "timeout_exceeded",
            "parallel_failed",
        ] {
            assert_eq!(anomaly_severity(&mk(failed, "done")), 2, "{failed}");
        }
        assert_eq!(anomaly_severity(&mk("none", "scheduled")), 1);
        for ok in ["passed", "softfailed", "obsoleted", "skipped"] {
            assert_eq!(anomaly_severity(&mk(ok, "done")), 0, "{ok}");
        }
    }

    /// `--test` keeps every matching job, passed ones included (#643).
    #[tokio::test]
    async fn test_glob_filter_keeps_matching_jobs_of_any_result() {
        let out = run_with(
            vec![
                job(1, "qam-incidentinstall", "passed", "done", DVD),
                job(2, "qam-incidentinstall-ha", "failed", "done", DVD),
                job(3, "mau-extratests", "failed", "done", DVD),
            ],
            &["--test", "*incidentinstall*"],
        )
        .await;
        assert_eq!(row_lines(&out).len(), 2, "{out}");
        assert!(out.contains("openQA jobs for build BUILD-42 (2)"), "{out}");
        assert!(!out.contains("mau-extratests"), "{out}");
    }

    /// `--flavor` and `--version` narrow by job settings, and every row shows
    /// the job's VERSION, FLAVOR and MACHINE (#643).
    #[tokio::test]
    async fn flavor_and_version_filters_and_rows_show_settings() {
        let jobs = vec![
            job(1, "dvd-test", "passed", "done", DVD),
            job(2, "micro-test", "passed", "done", MICRO),
        ];
        let out = run_with(jobs.clone(), &["--flavor", "Micro-Incidents"]).await;
        assert_eq!(row_lines(&out).len(), 1, "{out}");
        assert!(out.contains("micro-test"), "{out}");
        assert!(out.contains("[6.1 Micro-Incidents 64bit-2G]"), "{out}");

        let out = run_with(jobs, &["--version", "15-SP6"]).await;
        assert_eq!(row_lines(&out).len(), 1, "{out}");
        assert!(out.contains("dvd-test"), "{out}");
        assert!(out.contains("[15-SP6 Server-DVD-Incidents 64bit]"), "{out}");
    }

    /// `--offset`/`--limit` page any slice; windowing alone never prints a
    /// truncation notice, and the header still counts the full set (#643).
    #[tokio::test]
    async fn offset_and_limit_page_rows_without_notice() {
        let jobs: Vec<_> = (0..10)
            .map(|i| job(i, &format!("test-{i:02}"), "passed", "done", DVD))
            .collect();
        let out = run_with(jobs, &["--offset", "4", "--limit", "3"]).await;
        let rows = row_lines(&out);
        assert_eq!(rows.len(), 3, "{out}");
        assert!(rows[0].contains("test-04"), "{out}");
        assert!(rows[2].contains("test-06"), "{out}");
        assert!(out.contains("openQA jobs for build BUILD-42 (10)"), "{out}");
        assert!(!out.contains("truncated"), "{out}");
    }

    /// By default only the newest attempt of each scenario is listed and the
    /// earlier ones are counted; `--all` lists every attempt (#643).
    #[tokio::test]
    async fn default_lists_newest_attempt_and_all_lists_every_attempt() {
        let jobs = vec![
            job(1, "kdump", "failed", "done", DVD),
            job(2, "kdump", "failed", "done", DVD),
            job(3, "kdump", "passed", "done", DVD),
            job(4, "boot", "failed", "done", DVD),
            job(5, "boot", "obsoleted", "done", MICRO),
        ];
        let out = run_with(jobs.clone(), &[]).await;
        let rows = row_lines(&out);
        assert_eq!(rows.len(), 2, "{out}");
        assert!(rows.iter().any(|r| r.contains("/t3")), "{out}");
        assert!(rows.iter().any(|r| r.contains("/t4")), "{out}");
        assert!(out.contains("openQA jobs for build BUILD-42 (2)"), "{out}");
        assert!(
            out.contains("2 earlier attempts not listed (--all lists every attempt)"),
            "{out}"
        );

        assert!(!out.contains("not every job"), "{out}");

        let out = run_with(jobs, &["--all"]).await;
        assert_eq!(row_lines(&out).len(), 5, "{out}");
        assert!(!out.contains("earlier attempt"), "{out}");
    }

    /// An obsoleted newest job does not hide its scenario: the newest
    /// non-obsoleted attempt is listed, so `--failed` still shows the failure
    /// (#643 review).
    #[tokio::test]
    async fn obsoleted_newest_job_lists_the_newest_non_obsoleted_attempt() {
        let jobs = vec![
            job(1, "kdump", "passed", "done", DVD),
            job(2, "kdump", "failed", "done", DVD),
            job(3, "kdump", "obsoleted", "done", DVD),
        ];
        let out = run_with(jobs.clone(), &["--failed"]).await;
        let rows = row_lines(&out);
        assert_eq!(rows.len(), 1, "{out}");
        assert!(rows[0].contains("/t2"), "{out}");
        // The earlier attempt passed, so `--failed` does not count it.
        assert!(!out.contains("earlier attempt"), "{out}");

        // Without `--failed` it is counted; the obsoleted job never is.
        let out = run_with(jobs, &[]).await;
        assert_eq!(row_lines(&out).len(), 1, "{out}");
        assert!(out.contains("1 earlier attempt not listed"), "{out}");
    }

    /// A running newest attempt is the scenario's current state: `--failed`
    /// does not list the older failure it retries, and says the newest one is
    /// still pending and that an earlier attempt was left out.
    #[tokio::test]
    async fn pending_newest_attempt_hides_the_older_failure_from_failed() {
        let jobs = vec![
            job(1, "kdump", "failed", "done", DVD),
            job(2, "kdump", "none", "running", DVD),
        ];
        let out = run_with(jobs, &["--failed"]).await;
        assert!(row_lines(&out).is_empty(), "{out}");
        assert!(out.contains("1 still pending"), "{out}");
        assert!(out.contains("1 earlier attempt not listed"), "{out}");
    }

    /// A retriggered scenario whose newest attempt passed is not a failure:
    /// `--failed` finds nothing, and the note counts only the hidden failed
    /// attempts that match the other filters (#643).
    #[tokio::test]
    async fn failed_filter_counts_only_matching_hidden_attempts() {
        let jobs = vec![
            job(1, "kdump", "failed", "done", DVD),
            job(2, "kdump", "passed", "done", DVD),
            job(3, "other", "passed", "done", DVD),
            job(4, "other", "passed", "done", DVD),
        ];
        let out = run_with(jobs.clone(), &["--failed"]).await;
        assert!(row_lines(&out).is_empty(), "{out}");
        assert!(
            out.contains("1 earlier attempt not listed (--all lists every attempt)"),
            "{out}"
        );

        let out = run_with(jobs.clone(), &["--failed", "--all"]).await;
        assert_eq!(row_lines(&out).len(), 1, "{out}");

        let out = run_with(jobs, &["--test", "other"]).await;
        assert_eq!(row_lines(&out).len(), 1, "{out}");
        assert!(out.contains("1 earlier attempt not listed"), "{out}");
    }

    #[test]
    fn fetch_notes_say_what_is_left_out() {
        assert!(fetch_notes(0, true).is_empty());
        assert_eq!(
            fetch_notes(3, true),
            vec!["3 earlier attempts not listed (--all lists every attempt)"]
        );
        let notes = fetch_notes(0, false);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains(&oqa::MAX_INCIDENT_JOBS.to_string()),
            "{notes:?}"
        );
    }

    /// Over the row budget, at most `ROW_CAP` rows plus one notice are
    /// printed, the header counts every match, and failed and pending jobs
    /// survive ahead of passing ones (#643).
    #[tokio::test]
    async fn row_budget_caps_listing_and_keeps_failed_and_pending() {
        use super::super::row_budget::ROW_CAP;

        let mut jobs: Vec<_> = (0..140)
            .map(|i| job(i, &format!("pass-{i:03}"), "passed", "done", DVD))
            .collect();
        jobs.extend((0..10).map(|i| job(200 + i, &format!("fail-{i}"), "failed", "done", DVD)));
        jobs.extend((0..5).map(|i| job(300 + i, &format!("pend-{i}"), "none", "running", DVD)));

        let out = run_with(jobs, &[]).await;
        let rows = row_lines(&out);
        assert!(rows.len() <= ROW_CAP, "{} rows", rows.len());
        assert!(
            out.contains("openQA jobs for build BUILD-42 (155)"),
            "{out}"
        );
        for i in 0..10 {
            assert!(out.contains(&format!("fail-{i}")), "fail-{i} dropped");
        }
        for i in 0..5 {
            assert!(out.contains(&format!("pend-{i}")), "pend-{i} dropped");
        }
        let notices: Vec<_> = out.lines().filter(|l| l.contains("…[truncated")).collect();
        assert_eq!(notices.len(), 1, "{out}");
        assert!(notices[0].contains("(15/15 anomalies kept)"), "{out}");
        assert!(notices[0].contains(JOBS_HINT), "{out}");
    }

    /// Clears `$OPENQA_API_KEY`/`$OPENQA_API_SECRET` for the guard's lifetime:
    /// `ruoqa` 0.2 resolves that pair *ahead of* `client.conf`, so a test
    /// asserting a `client.conf` credential must not see an ambient shell one.
    /// Only valid inside `#[serial(openqa_config_env)]`.
    struct ApiEnvGuard {
        prev_key: Option<std::ffi::OsString>,
        prev_secret: Option<std::ffi::OsString>,
    }

    impl ApiEnvGuard {
        #[allow(unsafe_code)]
        fn new() -> Self {
            let prev_key = std::env::var_os("OPENQA_API_KEY");
            let prev_secret = std::env::var_os("OPENQA_API_SECRET");
            // SAFETY: guarded by `#[serial(openqa_config_env)]`.
            unsafe {
                std::env::remove_var("OPENQA_API_KEY");
                std::env::remove_var("OPENQA_API_SECRET");
            }
            Self {
                prev_key,
                prev_secret,
            }
        }
    }

    impl Drop for ApiEnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: guarded by `#[serial(openqa_config_env)]`.
            unsafe {
                match self.prev_key.take() {
                    Some(v) => std::env::set_var("OPENQA_API_KEY", v),
                    None => std::env::remove_var("OPENQA_API_KEY"),
                }
                match self.prev_secret.take() {
                    Some(v) => std::env::set_var("OPENQA_API_SECRET", v),
                    None => std::env::remove_var("OPENQA_API_SECRET"),
                }
            }
        }
    }

    /// `openqa_jobs` authenticates when `client.conf` has credentials for the
    /// instance, rather than relying on openQA accepting an unauthenticated GET.
    #[tokio::test]
    #[serial_test::serial(openqa_config_env)]
    // `set_var`/`remove_var` are `unsafe` in edition 2024; `#[serial]` makes the
    // process-global mutation exclusive.
    #[allow(unsafe_code)]
    async fn authenticates_against_an_instance_requiring_x_api_hash() {
        let _env = ApiEnvGuard::new();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"settings": {"BUILD": "BUILD-42", "DISTRI": "sle", "VERSION": "15-SP5"}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs"))
            .and(header("X-API-Key", "MYKEY"))
            .and(header_exists("X-API-Hash"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let host = server.uri();
        let host = host.split_once("://").map_or(host.as_str(), |(_, h)| h);
        std::fs::write(
            dir.path().join("client.conf"),
            format!("[{host}]\nkey = MYKEY\nsecret = MYSECRET\n"),
        )
        .unwrap();
        // SAFETY: serialised via `#[serial(openqa_config_env)]`.
        unsafe { std::env::set_var("OPENQA_CONFIG", dir.path()) };

        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(
            &OpenQAJobs,
            &[
                "--url-dashboard-qam",
                &server.uri(),
                "--url-openqa",
                &server.uri(),
            ],
        );
        let result = OpenQAJobs.call(&mut session, &args).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OPENQA_CONFIG") };
        result.unwrap();
        assert!(
            buf.contents().contains("No openQA jobs"),
            "{}",
            buf.contents()
        );
    }
}
