//! The `openqa_summary` command — the QEM Dashboard's openQA accounting.

use async_trait::async_trait;
use clap::ArgMatches;

use super::reload_openqa::ReloadOpenQA;
use crate::command::{Command, Scope};
use crate::commands::support::{require_update, template_completion};
use crate::error::CommandResult;
use crate::session::Session;

/// Prints the review-facing `Results from openQA jobs` block for the loaded MU.
///
/// The block is the dashboard's per-group accounting of the incident and
/// aggregate jobs: counts for each group and only the failing jobs listed. It
/// is what `export` writes into the report, rendered here without touching the
/// report document. The dashboard is queried on every call, so a still-running
/// group shows its current counts.
pub struct OpenQASummary;

#[async_trait]
impl Command for OpenQASummary {
    fn name(&self) -> &'static str {
        "openqa_summary"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Prints the QEM Dashboard's openQA results summary for the loaded MU.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        require_update(session)?;
        ReloadOpenQA.call(session, args).await?;

        let block = session
            .metadata()
            .openqa()
            .auto
            .as_ref()
            .map(|auto| auto.pp.concat())
            .unwrap_or_default();
        if block.trim().is_empty() {
            session
                .display
                .println("No openQA results on the QEM Dashboard for this update yet.");
        } else {
            session.display.println(block.trim_end());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;
    use mtui_types::Workflow;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(OpenQASummary.name(), "openqa_summary");
        assert_eq!(OpenQASummary.scope(), Scope::Fanout);
    }

    #[test]
    fn completion_offers_loaded_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = OpenQASummary.complete(&session, "SUSE", "");
        assert_eq!(out, vec!["SUSE:Maintenance:1:1"]);
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&OpenQASummary, &[]);
        let err = OpenQASummary.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// A dashboard answering for incident 1 with the given incident settings and
    /// the jobs of setting 7; the update settings are empty.
    async fn dashboard_server(jobs: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        let project = "SUSE:Maintenance:1";
        Mock::given(method("GET"))
            .and(path("/api/incidents/1"))
            .and(query_param("project", project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .and(query_param("project", project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": 7,
                "flavor": "Server-DVD-Incidents",
                "arch": "x86_64",
                "settings": {"DISTRI": "sle"}
            }])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/1"))
            .and(query_param("project", project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/incident/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jobs))
            .mount(&server)
            .await;
        server
    }

    fn job(id: u64, name: &str, status: &str) -> serde_json::Value {
        json!({
            "job_id": id,
            "name": name,
            "job_group": "Maintenance",
            "group_id": 1,
            "status": status,
            "distri": "sle",
            "flavor": "Server-DVD-Incidents",
            "arch": "x86_64",
            "version": "15-SP5",
            "build": ":1:bash",
            "obsolete": false
        })
    }

    async fn run(server: &MockServer) -> (Result<(), CommandError>, String) {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        let args = matches(&OpenQASummary, &[]);
        let result = OpenQASummary.call(&mut session, &args).await;
        (result, buf.contents())
    }

    #[tokio::test]
    async fn prints_the_dashboard_block_with_failing_jobs() {
        let server = dashboard_server(json!([
            job(1001, "qam-incidentinstall", "passed"),
            job(1002, "qam-bash", "failed"),
        ]))
        .await;

        let (result, out) = run(&server).await;
        result.unwrap();
        assert!(out.contains("Results from openQA jobs:"), "{out}");
        assert!(out.contains("failed: 1"), "{out}");
        assert!(out.contains("qam-bash"), "{out}");
    }

    #[tokio::test]
    async fn says_so_when_the_dashboard_has_no_jobs() {
        let server = dashboard_server(json!([])).await;

        let (result, out) = run(&server).await;
        result.unwrap();
        assert!(
            out.contains("No openQA results on the QEM Dashboard"),
            "{out}"
        );
        assert!(!out.contains("Results from openQA jobs:"), "{out}");
    }

    #[tokio::test]
    async fn dashboard_failure_is_an_error_not_an_empty_summary() {
        let server = MockServer::start().await;

        let (result, out) = run(&server).await;
        assert!(matches!(result, Err(CommandError::Other(_))));
        assert!(!out.contains("No openQA results"), "{out}");
    }
}
