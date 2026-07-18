//! Simple "set" commands (`set_log_level`, `set_workflow`).
//!
//! Ports upstream `mtui.commands.simpleset`. `set_timeout` already lives in
//! [`settimeout`](super::settimeout); the two "set" commands that remain here are
//! [`SetLogLevel`] and [`SetWorkflow`] (the latter reconstructs the report's
//! openQA results state when switching workflow).

use async_trait::async_trait;
use clap::{Arg, ArgMatches};
use mtui_types::Workflow;

use crate::command::{Command, Scope};
use crate::commands::support::{
    build_auto_openqa, build_incident, build_kernel_openqa, require_update, template_completion,
};
use crate::error::{CommandError, CommandResult};
use crate::session::{LogLevel, Session};

/// The workflow choices offered for completion / validation (upstream
/// `choices`).
const WORKFLOWS: &[&str] = &["auto", "manual", "kernel"];

/// The log levels offered for completion / validation (upstream `choices`).
const LEVELS: &[&str] = &["info", "warning", "error", "debug"];

/// Changes the current mtui log level (upstream `SetLogLevel`).
///
/// Sets the level on the session's installed log-level sink (the REPL wires this
/// to a `tracing_subscriber::reload` handle; headless callers still log the
/// change). Setting `debug` surfaces per-command tracing in real time.
pub struct SetLogLevel;

#[async_trait]
impl Command for SetLogLevel {
    fn name(&self) -> &'static str {
        "set_log_level"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Changes the current mtui log level.")
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("level")
                .required(true)
                .value_parser(clap::builder::PossibleValuesParser::new(LEVELS))
                .help("log level for mtui - info, warning, error or debug"),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        LEVELS
            .iter()
            .filter(|l| l.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let name = args
            .get_one::<String>("level")
            .ok_or_else(|| CommandError::Other("log level is required".to_owned()))?;
        let level = LogLevel::parse(name)
            .ok_or_else(|| CommandError::Other(format!("unknown log level: {name}")))?;
        session.apply_log_level(level);
        session.display.println(&format!("Log level set to {name}"));
        Ok(())
    }
}

/// Sets the workflow and reloads data from openQA (upstream
/// `mtui.commands.simpleset.SetWorkflow`).
///
/// Reconstructs the report's openQA holder for the requested workflow:
///
/// * `kernel` — rebuild the "auto" result and the two per-instance kernel
///   results.
/// * `auto` — rebuild the "auto" result; if it has no install jobs (or they
///   failed) the workflow is auto-downgraded to `manual`.
/// * `manual` — refresh the "auto" result and clear the kernel results.
///
/// When the requested workflow equals the current one, the existing results are
/// merely refreshed. Requires a loaded update.
pub struct SetWorkflow;

#[async_trait]
impl Command for SetWorkflow {
    fn name(&self) -> &'static str {
        "set_workflow"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Sets the workflow and reloads data from openQA.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("workflow")
                .required(true)
                .value_parser(clap::builder::PossibleValuesParser::new(WORKFLOWS))
                .help("desired workflow - auto, manual or kernel"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let mut out: Vec<String> = WORKFLOWS
            .iter()
            .filter(|w| w.starts_with(text))
            .map(|s| (*s).to_owned())
            .collect();
        out.extend(template_completion(session, text));
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let desired: Workflow = args
            .get_one::<String>("workflow")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| CommandError::Other("workflow is required".to_owned()))?;

        // Snapshot config primitives so no `&Session` borrow crosses `.await`.
        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;
        let dashboard_api = session.config.qem_dashboard_api.clone();
        let openqa_instance = session.config.openqa_instance.clone();
        let openqa_baremetal = session.config.openqa_instance_baremetal.clone();
        let current = session.metadata().workflow();

        let incident = build_incident(rrid.clone(), dashboard_api, http.clone()).await;

        match desired {
            Workflow::Kernel => {
                if current == Workflow::Kernel {
                    tracing::info!("Desired workflow kernel is same as current");
                    refresh_auto(session, &incident, &openqa_instance, rrid.clone()).await;
                    let stale = std::mem::take(&mut session.metadata_mut().openqa_mut().kernel);
                    let mut refreshed = Vec::with_capacity(stale.len());
                    for oqa in stale {
                        refreshed.push(oqa.run().await);
                    }
                    session.metadata_mut().openqa_mut().kernel = refreshed;
                    print_workflow(session);
                    return Ok(());
                }
                tracing::info!("Setting workflow to 'kernel'");
                session.set_workflow(Workflow::Kernel);
                let mut auto = build_auto_openqa(openqa_instance.clone(), &incident, rrid.clone());
                auto.run().await;
                session.metadata_mut().openqa_mut().auto = Some(auto);
                let mut kernel = Vec::new();
                for host in [openqa_instance, openqa_baremetal] {
                    kernel.push(
                        build_kernel_openqa(&incident, &host, http.clone())
                            .run()
                            .await,
                    );
                }
                session.metadata_mut().openqa_mut().kernel = kernel;
            }
            Workflow::Auto => {
                if current == Workflow::Auto {
                    tracing::info!("Desired workflow auto is same as current");
                    refresh_auto(session, &incident, &openqa_instance, rrid).await;
                    print_workflow(session);
                    return Ok(());
                }
                tracing::info!("Setting workflow to 'auto'");
                session.set_workflow(Workflow::Auto);
                let mut auto = build_auto_openqa(openqa_instance, &incident, rrid);
                auto.run().await;
                let no_results = auto.results.is_none();
                session.metadata_mut().openqa_mut().auto = Some(auto);
                session.metadata_mut().openqa_mut().kernel = Vec::new();
                if no_results {
                    tracing::warn!("No install jobs or install jobs failed");
                    let msg = session
                        .display
                        .yellow("No install jobs or install jobs failed; switching mode to manual");
                    session.display.println(&msg);
                    session.set_workflow(Workflow::Manual);
                }
            }
            Workflow::Manual => {
                if current == Workflow::Manual {
                    tracing::info!("Desired workflow manual is same as current");
                    refresh_auto(session, &incident, &openqa_instance, rrid).await;
                    print_workflow(session);
                    return Ok(());
                }
                tracing::info!("Setting workflow to 'manual'");
                session.set_workflow(Workflow::Manual);
                refresh_auto(session, &incident, &openqa_instance, rrid).await;
                session.metadata_mut().openqa_mut().kernel = Vec::new();
            }
        }

        print_workflow(session);
        Ok(())
    }
}

/// Prints the report's resulting workflow to the session display, so the caller
/// (REPL/MCP) sees the outcome rather than an empty success. The openQA
/// connectors used above do not expose a fetch-failure signal (a failed fetch is
/// swallowed as "no jobs" by `DashboardAutoOpenQA::run`), so a network failure
/// still resolves to `manual` here rather than surfacing as `Err`.
fn print_workflow(session: &mut Session) {
    let workflow = session.metadata().workflow();
    session
        .display
        .println(&format!("Workflow set to '{workflow}'"));
}

/// Refreshes the report's "auto" openQA result in place, building a fresh one
/// when the holder is empty.
///
/// Upstream's same-workflow branches call `metadata.openqa.auto.run()`
/// unconditionally, assuming `auto` was populated at load time. The Rust holder
/// starts empty, so this guards the `None` case by building and running a fresh
/// connector (the "get" semantics) rather than panicking.
async fn refresh_auto(
    session: &mut Session,
    incident: &mtui_datasources::qem_dashboard::incident::QemIncident,
    openqa_instance: &str,
    rrid: mtui_types::RequestReviewID,
) {
    if let Some(auto) = session.metadata_mut().openqa_mut().auto.as_mut() {
        auto.run().await;
    } else {
        let mut auto = build_auto_openqa(openqa_instance.to_owned(), incident, rrid);
        auto.run().await;
        session.metadata_mut().openqa_mut().auto = Some(auto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use std::sync::{Arc, Mutex};

    #[test]
    fn name_is_set_log_level() {
        assert_eq!(SetLogLevel.name(), "set_log_level");
    }

    #[test]
    fn rejects_unknown_level_at_parse_time() {
        let base = clap::Command::new("set_log_level").no_binary_name(true);
        let cmd = SetLogLevel.configure(base);
        assert!(cmd.clone().try_get_matches_from(["bogus"]).is_err());
        assert!(cmd.clone().try_get_matches_from([] as [&str; 0]).is_err());
        assert!(cmd.try_get_matches_from(["debug"]).is_ok());
    }

    #[test]
    fn completion_filters_levels_by_prefix() {
        let (session, _buf) = empty_session();
        assert_eq!(SetLogLevel.complete(&session, "de", ""), vec!["debug"]);
        let mut all = SetLogLevel.complete(&session, "", "");
        all.sort();
        assert_eq!(all, vec!["debug", "error", "info", "warning"]);
    }

    #[tokio::test]
    async fn applies_level_through_installed_sink() {
        let (mut session, buf) = empty_session();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        session.set_log_level_sink(Box::new(move |lvl| sink_seen.lock().unwrap().push(lvl)));

        let args = matches(&SetLogLevel, &["warning"]);
        SetLogLevel.call(&mut session, &args).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![LogLevel::Warning]);
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("Log level set to warning"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn succeeds_without_sink_installed() {
        let (mut session, _buf) = empty_session();
        let args = matches(&SetLogLevel, &["debug"]);
        // No sink installed (headless): still Ok, just logs.
        SetLogLevel.call(&mut session, &args).await.unwrap();
    }

    // --- SetWorkflow ---

    #[test]
    fn set_workflow_name_and_scope() {
        assert_eq!(SetWorkflow.name(), "set_workflow");
        assert_eq!(SetWorkflow.scope(), Scope::Fanout);
    }

    #[test]
    fn set_workflow_rejects_unknown_choice() {
        let base = clap::Command::new("set_workflow").no_binary_name(true);
        let cmd = SetWorkflow.configure(base);
        assert!(cmd.clone().try_get_matches_from(["bogus"]).is_err());
        assert!(cmd.clone().try_get_matches_from([] as [&str; 0]).is_err());
        assert!(cmd.try_get_matches_from(["auto"]).is_ok());
    }

    #[test]
    fn set_workflow_completion_offers_choices_and_templates() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert_eq!(SetWorkflow.complete(&session, "ke", ""), vec!["kernel"]);
        let out = SetWorkflow.complete(&session, "SUSE", "");
        assert_eq!(out, vec!["SUSE:Maintenance:1:1"]);
    }

    #[tokio::test]
    async fn set_workflow_errors_when_no_report_loaded() {
        let (mut session, _buf) = empty_session();
        let args = matches(&SetWorkflow, &["auto"]);
        let err = SetWorkflow.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// Mounts the three QEM-dashboard endpoints the auto path touches, each
    /// returning an empty-but-valid body.
    async fn dashboard_server(incident_number: &str) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        for (endpoint, body) in [
            ("incidents", serde_json::json!({})),
            ("incident_settings", serde_json::json!([])),
            ("update_settings", serde_json::json!([])),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/api/{endpoint}/{incident_number}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        server
    }

    fn point_at(session: &mut Session, server: &wiremock::MockServer) {
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        session.config.openqa_instance_baremetal = server.uri();
    }

    #[tokio::test]
    async fn auto_with_no_install_jobs_downgrades_to_manual() {
        // Empty settings → no install results → upstream switches to manual.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Manual);
        let server = dashboard_server("1").await;
        point_at(&mut session, &server);

        let args = matches(&SetWorkflow, &["auto"]);
        SetWorkflow.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
        assert!(session.metadata().openqa().auto.is_some());
        assert!(session.metadata().openqa().kernel.is_empty());
        // The downgrade and the resulting workflow reach the display, not just logs.
        let out = buf.contents();
        assert!(out.contains("switching mode to manual"), "{out}");
        assert!(out.contains("Workflow set to 'manual'"), "{out}");
    }

    #[tokio::test]
    async fn kernel_populates_auto_and_two_kernel_results() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Manual);
        let server = dashboard_server("1").await;
        point_at(&mut session, &server);
        // The kernel connectors hit openQA's /api/v1/jobs; return no jobs.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/jobs"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"jobs": []})),
            )
            .mount(&server)
            .await;

        let args = matches(&SetWorkflow, &["kernel"]);
        SetWorkflow.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Kernel);
        assert!(session.metadata().openqa().auto.is_some());
        assert_eq!(session.metadata().openqa().kernel.len(), 2);
    }

    #[tokio::test]
    async fn manual_same_workflow_refreshes_auto() {
        // Same-workflow manual: upstream only refreshes `auto` and returns
        // (kernel is left untouched). refresh_auto builds a fresh auto when the
        // holder was empty.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Manual);
        let server = dashboard_server("1").await;
        point_at(&mut session, &server);

        let args = matches(&SetWorkflow, &["manual"]);
        SetWorkflow.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
        assert!(session.metadata().openqa().auto.is_some());
        // Same-workflow branch still reports the resulting workflow to display.
        assert!(buf.contents().contains("Workflow set to 'manual'"));
    }

    #[tokio::test]
    async fn switch_to_manual_from_auto_clears_kernel() {
        // Transitioning INTO manual (not same-workflow) clears kernel results.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.set_workflow(Workflow::Auto);
        let server = dashboard_server("1").await;
        point_at(&mut session, &server);
        // Pre-seed a kernel result to prove it gets cleared.
        let rrid = session.metadata().rrid().unwrap().clone();
        let dashboard_api = session.config.qem_dashboard_api.clone();
        let host = session.config.openqa_instance.clone();
        let http = session.http_client().unwrap();
        let incident = build_incident(rrid, dashboard_api, http.clone()).await;
        session
            .metadata_mut()
            .openqa_mut()
            .kernel
            .push(build_kernel_openqa(&incident, &host, http));

        let args = matches(&SetWorkflow, &["manual"]);
        SetWorkflow.call(&mut session, &args).await.unwrap();
        assert_eq!(session.metadata().workflow(), Workflow::Manual);
        assert!(session.metadata().openqa().kernel.is_empty());
    }
}
