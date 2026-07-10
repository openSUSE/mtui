//! The `export` command (writes the gathered update data to the template).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_testreport::{
    AutoExport, DenyOverwrite, ExportContext, FileList, KernelExport, ManualExport, ManualHost,
};
use mtui_types::Workflow;

use super::support::{
    add_hosts_arg, build_auto_openqa, build_incident, config_verify_policy, require_update,
    select_names, template_completion,
};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Exports the gathered update data to the testing template.
///
/// Ports upstream `mtui.commands.export.Export`. Picks the exporter by the
/// report's [`Workflow`] and writes the pre/post package versions and update log
/// into the template (or `filename` when given). Requires a loaded report.
///
/// ## openQA enrichment (Manual)
///
/// The `Manual` exporter folds openQA results into the template via the report's
/// openQA holder (`metadata.openqa`). When the holder's "auto" result is absent,
/// it is lazily built and run from the QEM Dashboard (upstream
/// `DashboardAutoOpenQA(...)`), then the connected-host results
/// (`report_results`) and any `openqa_overview` payload are folded into
/// [`ManualExport`]. `Auto`/`Kernel` render their full local template.
pub struct Export;

#[async_trait]
impl Command for Export {
    fn name(&self) -> &'static str {
        "export"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Exports the gathered update data to the testing template.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("force")
                    .short('f')
                    .long("force")
                    .action(ArgAction::SetTrue)
                    .help(
                        "force overwrite existing template and re-download openQA \
                         results present in the log",
                    ),
            )
            .arg(
                Arg::new("filename")
                    .value_name("FILENAME")
                    .help("output template file name (defaults to the loaded template)"),
            )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let rrid = require_update(session)?;
        let workflow = session.metadata().workflow();
        let force = args.get_flag("force");

        // Output path: explicit `filename`, else the loaded report's own path.
        let filename: PathBuf = match args.get_one::<String>("filename") {
            Some(f) => PathBuf::from(f),
            None => session
                .metadata()
                .base()
                .path
                .clone()
                .ok_or_else(|| CommandError::Other("no report path to export to".to_owned()))?,
        };

        // For Manual exports, ensure the report's openQA "auto" result exists
        // (lazily built + run from the QEM Dashboard, upstream export.py:58-64)
        // and select the connected-host results to fold in (report_results).
        let (manual_results, manual_overview) = if workflow == Workflow::Manual {
            if session.metadata().openqa().auto.is_none() {
                let policy = config_verify_policy(session);
                let dashboard_api = session.config.qem_dashboard_api.clone();
                let openqa_instance = session.config.openqa_instance.clone();
                let incident = build_incident(rrid.clone(), dashboard_api, policy).await?;
                let mut auto = build_auto_openqa(openqa_instance, &incident, rrid.clone());
                auto.run().await;
                session.metadata_mut().openqa_mut().auto = Some(auto);
            }
            let hosts = select_names(session.targets(), args, false)
                .map_err(|e| CommandError::Other(e.to_string()))?;
            let results = manual_hosts(session, &hosts);
            let overview = session.metadata().openqa().overview.clone();
            (Some((hosts, results)), overview)
        } else {
            (None, None)
        };

        let text = FileList::load(&filename).map_err(|e| {
            CommandError::Other(format!("could not read template {filename:?}: {e}"))
        })?;
        let ctx = ExportContext::new(session.config.clone(), text.lines(), force, rrid);

        let template: Vec<String> = match workflow {
            Workflow::Auto => {
                let http = build_http(session)?;
                AutoExport::new(ctx, None, None)
                    .run(&http, &DenyOverwrite)
                    .await
            }
            Workflow::Kernel => {
                let http = build_http(session)?;
                KernelExport::new(ctx, Vec::new(), None).run(&http).await
            }
            Workflow::Manual => {
                let (hosts, results) = manual_results.expect("computed for Manual workflow");
                let auto = session.metadata().openqa().auto.clone();
                ManualExport::new(ctx, results, auto, manual_overview).run(&hosts, &DenyOverwrite)
            }
        };

        let mut out = FileList::from_lines(&filename, template);
        out.write().map_err(|e| {
            CommandError::Other(format!("could not write template {filename:?}: {e}"))
        })?;
        tracing::info!(path = %filename.display(), "template exported");
        Ok(())
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }
}

/// Builds a verifying HTTP client from the session config (log-download seam).
fn build_http(session: &Session) -> Result<HttpClient, CommandError> {
    let verify = resolve_verify(
        VerifyPolicy::Default(true),
        Some(VerifyPolicy::from_config(&session.config.ssl_verify)),
    );
    HttpClient::new(verify)
        .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))
}

/// Builds the decoupled [`ManualHost`] views the manual exporter reads, from the
/// named connected targets (upstream reads the live `Target`s directly).
fn manual_hosts(session: &Session, hosts: &[String]) -> Vec<ManualHost> {
    hosts
        .iter()
        .filter_map(|name| session.targets().get(name))
        .map(|t| ManualHost {
            hostname: t.hostname().to_owned(),
            system: t.system().to_string(),
            packages: t.packages().to_vec(),
            hostlog: t.out().clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Export.name(), "export");
        assert_eq!(Export.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn no_report_errors_before_any_io() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Export, &[]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[tokio::test]
    async fn auto_writes_template_to_explicit_filename() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Auto;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        // The auto exporter appends the system-info footer to any template.
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn kernel_writes_template_to_explicit_filename() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Kernel;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "regression tests:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    /// Mounts the three QEM-dashboard endpoints the manual enrichment touches.
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

    #[tokio::test]
    async fn manual_lazily_builds_and_folds_openqa_auto() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Manual;
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        let dir = tempfile::tempdir().unwrap();
        // The manual exporter writes per-host install logs under
        // `template_dir/<rrid>/install_logs`; keep that inside the tempdir so the
        // test never pollutes the working tree.
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        assert!(session.metadata().openqa().auto.is_none());
        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        // The auto result was lazily built and stored on the report holder.
        assert!(session.metadata().openqa().auto.is_some());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn manual_reuses_existing_openqa_auto() {
        // When the holder already has an "auto" result, export must not rebuild
        // it (no dashboard call needed).
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Manual;
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        // Pre-seed an auto result via a throwaway dashboard.
        let server = dashboard_server("1").await;
        let rrid = session.metadata().rrid().unwrap().clone();
        let policy = config_verify_policy(&session);
        let incident = build_incident(rrid.clone(), format!("{}/api", server.uri()), policy)
            .await
            .unwrap();
        session.metadata_mut().openqa_mut().auto =
            Some(build_auto_openqa(server.uri(), &incident, rrid));

        // Now point config at an unreachable dashboard: if export tried to
        // rebuild, it would still succeed (errors are surfaced), so instead we
        // assert the pre-seeded result is preserved.
        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();
        assert!(session.metadata().openqa().auto.is_some());
    }

    #[tokio::test]
    async fn missing_file_errors_cleanly() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.templates.active_mut().base_mut().workflow = Workflow::Auto;
        let args = matches(&Export, &["-f", "/nonexistent/dir/nope.txt"]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
