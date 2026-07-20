//! The `reload_openqa` command — refresh the report's openQA results.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_types::Workflow;

use crate::command::{Command, Scope};
use crate::commands::support::{
    build_auto_openqa, build_incident, build_kernel_openqa, require_update, template_completion,
};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Reloads information from the openQA instances (upstream
/// `mtui.commands.reloadoqa.ReloadOpenQA`).
///
/// For a kernel-workflow report the per-instance kernel results are (re)fetched
/// from the primary and the baremetal openQA instances; the QEM-dashboard "auto"
/// result is (re)fetched for every workflow. Results are stored on the report's
/// openQA holder ([`TestReport::openqa_mut`](mtui_testreport::TestReport)).
/// Requires a loaded update.
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
        // Snapshot the config primitives up front so no `&Session` borrow is
        // held across an `.await` (`Session` is not `Sync`).
        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;
        let dashboard_api = session.config.qem_dashboard_api.clone();
        let openqa_instance = session.config.openqa_instance.clone();
        let openqa_baremetal = session.config.openqa_instance_baremetal.clone();
        let workflow = session.metadata().workflow();

        let incident = build_incident(rrid.clone(), dashboard_api, http.clone()).await;

        if workflow == Workflow::Kernel {
            if session.metadata().openqa().kernel.is_empty() {
                tracing::info!("Getting data from kernel openQA");
                for host in [openqa_instance.clone(), openqa_baremetal] {
                    let oqa = build_kernel_openqa(&incident, &host, http.clone())
                        .run()
                        .await
                        .map_err(openqa_fetch_err)?;
                    session.metadata_mut().openqa_mut().kernel.push(oqa);
                }
            } else {
                tracing::info!("Refreshing data from kernel openQA");
                // `KernelOpenQA::run` consumes `self`, so drain-and-rebuild.
                let stale = std::mem::take(&mut session.metadata_mut().openqa_mut().kernel);
                let mut refreshed = Vec::with_capacity(stale.len());
                for oqa in stale {
                    refreshed.push(oqa.run().await.map_err(openqa_fetch_err)?);
                }
                session.metadata_mut().openqa_mut().kernel = refreshed;
            }
        }

        if session.metadata().openqa().auto.is_none() {
            tracing::info!("Getting data from QEM Dashboard");
            let mut auto = build_auto_openqa(
                openqa_instance,
                &incident,
                rrid,
                session.config.max_parallel as usize,
            );
            auto.run().await.map_err(dashboard_fetch_err)?;
            session.metadata_mut().openqa_mut().auto = Some(auto);
        } else {
            tracing::info!("Refreshing data from QEM Dashboard");
            if let Some(auto) = session.metadata_mut().openqa_mut().auto.as_mut() {
                auto.run().await.map_err(dashboard_fetch_err)?;
            }
        }

        Ok(())
    }
}

/// Map a QEM Dashboard fetch failure to a user-facing command error.
fn dashboard_fetch_err(e: mtui_datasources::QemDashboardError) -> CommandError {
    CommandError::Other(format!(
        "could not fetch openQA data from QEM Dashboard: {e}"
    ))
}

/// Map an openQA jobs fetch failure to a user-facing command error.
fn openqa_fetch_err(e: mtui_datasources::OpenQAError) -> CommandError {
    CommandError::Other(format!("could not fetch openQA data: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

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

    /// Mounts the three QEM-dashboard endpoints the auto path touches, each
    /// returning an empty-but-valid body, so `DashboardAutoOpenQA::run` resolves
    /// to a populated-but-empty result (no install jobs).
    async fn dashboard_server(incident_number: &str) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/incidents/{incident_number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/incident_settings/{incident_number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/update_settings/{incident_number}")))
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
        // The auto holder is now present (install jobs were empty → no results).
        assert!(session.metadata().openqa().auto.is_some());
        assert!(session.metadata().openqa().kernel.is_empty());
    }

    #[tokio::test]
    async fn auto_fetch_failure_returns_err() {
        // Dashboard unreachable (no mounts -> settings 404): reload must surface
        // the failure as Err rather than folding to an empty auto holder.
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
        let http = session.http_client().unwrap();
        let dashboard_api = session.config.qem_dashboard_api.clone();
        let openqa_instance = session.config.openqa_instance.clone();
        let incident = build_incident(rrid.clone(), dashboard_api, http).await;
        let max_parallel = session.config.max_parallel as usize;
        session.metadata_mut().openqa_mut().auto = Some(build_auto_openqa(
            openqa_instance,
            &incident,
            rrid,
            max_parallel,
        ));

        let args = matches(&ReloadOpenQA, &[]);
        ReloadOpenQA.call(&mut session, &args).await.unwrap();
        assert!(session.metadata().openqa().auto.is_some());
    }
}
