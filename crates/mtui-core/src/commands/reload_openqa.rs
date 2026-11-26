//! The `reload_openqa` command — refresh the report's openQA results.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_types::Workflow;

use crate::command::{Command, Scope};
use crate::commands::support::{
    OpenqaCtx, fetch_kernel, refresh_auto, refresh_kernel, require_update,
    session_openqa_transport, template_completion,
};
use crate::error::CommandResult;
use crate::session::Session;

/// Reloads information from the openQA instances onto the report's holder
/// ([`TestReport::openqa_mut`](mtui_testreport::TestReport)).
///
/// A kernel-workflow report (re)fetches its per-instance kernel results from the
/// primary and baremetal instances; the QEM-dashboard "auto" result is
/// (re)fetched for every workflow. Requires a loaded update.
pub struct ReloadOpenQA;

#[async_trait]
impl Command for ReloadOpenQA {
    fn name(&self) -> &'static str {
        "reload_openqa"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Reloads information from the openQA instances.")
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

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        // Snapshotted so no `&Session` borrow crosses an `.await`; it is not
        // `Sync`.
        let ctx = OpenqaCtx::snapshot(session, rrid)?;
        let openqa_transport = session_openqa_transport(session)?;
        let workflow = session.metadata().workflow();
        let incident = ctx.incident().await;

        if workflow == Workflow::Kernel {
            if session.metadata().openqa().kernel.is_empty() {
                tracing::info!("Getting data from kernel openQA");
                fetch_kernel(
                    &ctx,
                    &incident,
                    &openqa_transport,
                    &mut session.metadata_mut().openqa_mut().kernel,
                )
                .await?;
            } else {
                tracing::info!("Refreshing data from kernel openQA");
                refresh_kernel(session).await?;
            }
        }

        if session.metadata().openqa().auto.is_none() {
            tracing::info!("Getting data from QEM Dashboard");
        } else {
            tracing::info!("Refreshing data from QEM Dashboard");
        }
        refresh_auto(session, &ctx, &incident).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ReloadOpenQA.name(), "reload_openqa");
        assert_eq!(ReloadOpenQA.scope(), Scope::Fanout);
    }

    #[test]
    fn completion_offers_loaded_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let out = ReloadOpenQA.complete(&session, "SUSE", "");
        assert_eq!(out, vec!["SUSE:Maintenance:1:1"]);
    }

    #[tokio::test]
    async fn errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ReloadOpenQA, &[]);
        let err = ReloadOpenQA.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// The three QEM-dashboard endpoints the auto path touches, each returning
    /// an empty-but-valid body, so `DashboardAutoOpenQA::run` resolves to a
    /// populated-but-empty result.
    async fn dashboard_server(incident_number: &str) -> wiremock::MockServer {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let project = format!("SUSE:Maintenance:{incident_number}");
        Mock::given(method("GET"))
            .and(path(format!("/api/incidents/{incident_number}")))
            .and(query_param("project", &project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/incident_settings/{incident_number}")))
            .and(query_param("project", &project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/update_settings/{incident_number}")))
            .and(query_param("project", &project))
            .and(query_param("type", "smelt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn auto_workflow_populates_auto_holder() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();

        assert!(session.metadata().openqa().auto.is_none());
        let args = matches(&ReloadOpenQA, &[]);
        ReloadOpenQA.call(&mut session, &args).await.unwrap();
        assert!(session.metadata().openqa().auto.is_some());
        assert!(session.metadata().openqa().kernel.is_empty());
    }

    #[tokio::test]
    async fn auto_fetch_failure_returns_err() {
        // An unreachable dashboard must surface as Err, not fold to an empty
        // auto holder.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let server = wiremock::MockServer::start().await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();

        let args = matches(&ReloadOpenQA, &[]);
        let err = ReloadOpenQA.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn auto_workflow_refreshes_existing_auto_in_place() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();

        // Prime the holder so the "refresh" branch runs instead of "get".
        let rrid = session.metadata().rrid().unwrap().clone();
        let ctx = OpenqaCtx::snapshot(&session, rrid).unwrap();
        let incident = ctx.incident().await;
        session.metadata_mut().openqa_mut().auto = Some(ctx.auto(&incident));

        let args = matches(&ReloadOpenQA, &[]);
        ReloadOpenQA.call(&mut session, &args).await.unwrap();
        assert!(session.metadata().openqa().auto.is_some());
    }

    async fn mount_empty_jobs(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/jobs"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn kernel_workflow_fetches_both_instances_then_refreshes_them_in_place() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Kernel);
        let server = dashboard_server("1").await;
        mount_empty_jobs(&server).await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        session.config.openqa_instance_baremetal = server.uri();

        let args = matches(&ReloadOpenQA, &[]);
        ReloadOpenQA.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().openqa().kernel.len(), 2);
        ReloadOpenQA.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().openqa().kernel.len(), 2);
    }

    #[tokio::test]
    async fn kernel_fetch_failure_keeps_the_instances_fetched_before_it() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Kernel);
        let server = dashboard_server("1").await;
        mount_empty_jobs(&server).await;
        let broken = wiremock::MockServer::start().await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        session.config.openqa_instance_baremetal = broken.uri();

        let args = matches(&ReloadOpenQA, &[]);
        let err = ReloadOpenQA.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.starts_with("could not fetch openQA data: ")),
            "{err:?}"
        );
        assert_eq!(session.metadata().openqa().kernel.len(), 1);
    }

    #[tokio::test]
    async fn dashboard_failure_names_the_dashboard() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let server = wiremock::MockServer::start().await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();

        let args = matches(&ReloadOpenQA, &[]);
        let err = ReloadOpenQA.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m)
                if m.starts_with("could not fetch openQA data from QEM Dashboard: ")),
            "{err:?}"
        );
    }
}
