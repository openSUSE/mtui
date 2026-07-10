//! The `openqa_jobs` command — list the individual openQA jobs for an update.

use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::oqa_search as oqa;
use mtui_datasources::{HttpClient, VerifyPolicy, resolve_verify};

use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// openQA results that count as "not a failure" for the `--failed` filter
/// (upstream `_PASSING`).
const PASSING: &[&str] = &["passed", "softfailed"];
/// openQA results that are neutral (neither pass nor fail; upstream `_NEUTRAL`).
const NEUTRAL: &[&str] = &["obsoleted", "skipped"];

/// Lists the individual openQA jobs for the loaded update's incident build.
///
/// Ports upstream `mtui.commands.openqa_jobs.OpenQAJobs`. By default `obsoleted`
/// jobs are dropped; `--all` keeps them, `--failed` shows only non-passing jobs,
/// and `--arch` filters by architecture. Requires a loaded update.
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
                .help("keep obsoleted jobs (superseded by a retrigger)"),
        )
        .arg(
            Arg::new("failed")
                .long("failed")
                .action(ArgAction::SetTrue)
                .help("show only non-passing jobs (failed / parallel_failed / incomplete)"),
        )
        .arg(
            Arg::new("arch")
                .long("arch")
                .value_name("ARCH")
                .help("only jobs for this architecture"),
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

        let include_obsoleted = args.get_flag("all");
        let only_failed = args.get_flag("failed");
        let arch_filter = args.get_one::<String>("arch").cloned();

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

        let verify = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&session.config.ssl_verify)),
        );
        let http = HttpClient::new(verify)
            .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;

        // incident_id is maintenance_id (int for Maintenance, "1.2" for SLFO);
        // fall back to the review id in the SLFO case — mirrors openqa_overview.
        let effective_incident_id = if rrid.maintenance_id.parse::<i64>().is_ok() {
            rrid.maintenance_id.clone()
        } else {
            rrid.review_id.to_string()
        };

        let (build, _versions) =
            match oqa::get_incident_info(&http, &url_dashboard_qam, &effective_incident_id).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Failed to query QEM Dashboard: {e}");
                    return Ok(());
                }
            };

        let mut jobs = match oqa::incident_jobs(&http, &build, &url_openqa, include_obsoleted).await
        {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("Failed to query openQA jobs: {e}");
                return Ok(());
            }
        };
        if let Some(arch) = &arch_filter {
            jobs.retain(|j| &j.arch == arch);
        }
        if only_failed {
            jobs.retain(|j| {
                !PASSING.contains(&j.result.as_str()) && !NEUTRAL.contains(&j.result.as_str())
            });
        }

        if jobs.is_empty() {
            let msg = session
                .display
                .yellow(&format!("No openQA jobs for build {build:?}"));
            session.display.println(&msg);
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
        session.display.println("");
        for j in &jobs {
            let result = if PASSING.contains(&j.result.as_str()) {
                session.display.green(&format!("{:<15}", j.result))
            } else if NEUTRAL.contains(&j.result.as_str()) {
                session.display.yellow(&format!("{:<15}", j.result))
            } else {
                session.display.red(&format!("{:<15}", j.result))
            };
            session
                .display
                .println(&format!("  {result} {:<8} {}  {}", j.arch, j.test, j.url));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use wiremock::matchers::{method, path, query_param};
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
        assert_eq!(out, vec!["--failed"]);
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
            .respond_with(ResponseTemplate::new(200).set_body_json(jobs))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn lists_jobs_with_summary() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "install", "arch": "x86_64", "result": "passed", "clone_id": null},
                {"id": 2, "test": "boot", "arch": "x86_64", "result": "failed", "clone_id": null},
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
    }

    #[tokio::test]
    async fn failed_filter_drops_passing_jobs() {
        let server = server_with_jobs(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "install", "arch": "x86_64", "result": "passed", "clone_id": null},
                {"id": 2, "test": "boot", "arch": "aarch64", "result": "failed", "clone_id": null},
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
}
