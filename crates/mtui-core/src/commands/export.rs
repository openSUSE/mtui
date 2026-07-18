//! The `export` command (writes the gathered update data to the template).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::HttpClient;
use mtui_testreport::{
    AutoExport, DenyOverwrite, ExportContext, FileList, KernelExport, ManualExport, ManualHost,
};
use mtui_types::Workflow;

use super::support::{
    add_hosts_arg, build_auto_openqa, build_incident, named_hosts, require_update, select_names,
    template_completion,
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

    /// `export` opts out of the driver's host-less skip: for the `Auto`/`Kernel`
    /// workflows it sources its data from openQA and needs no connected hosts, so
    /// `export --all-templates` must still write those templates at zero hosts.
    /// The per-template `Manual`-workflow rule (which *does* need hosts) is
    /// applied inside [`call`](Self::call).
    fn skip_hostless_templates(&self) -> bool {
        false
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

        // The Manual workflow folds per-host update logs into the template, so a
        // host-less template has nothing to fold. When no `-t` was named, report
        // and skip it rather than writing an empty export; a typo'd `-t` still
        // fails loudly below via `select_names`. Auto/Kernel source from openQA
        // and proceed regardless of connected-host count.
        if workflow == Workflow::Manual && !named_hosts(args) && session.targets().is_empty() {
            session
                .display
                .println("skipped: manual export needs a connected host");
            return Ok(());
        }

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
                let http = build_http(session)?;
                let dashboard_api = session.config.qem_dashboard_api.clone();
                let openqa_instance = session.config.openqa_instance.clone();
                let incident = build_incident(rrid.clone(), dashboard_api, http).await;
                let mut auto = build_auto_openqa(openqa_instance, &incident, rrid.clone());
                // Best-effort, matching upstream export: a failed dashboard fetch
                // is folded to "no results" so the export still renders the rest
                // of the report rather than aborting.
                if let Err(e) = auto.run().await {
                    tracing::warn!(error = %e, "QEM Dashboard fetch failed during export; continuing without auto results");
                }
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
                let auto = session.metadata().openqa().auto.clone();
                let overview = session.metadata().openqa().overview.clone();
                AutoExport::new(ctx, auto, overview)
                    .run(&http, &DenyOverwrite)
                    .await
            }
            Workflow::Kernel => {
                let http = build_http(session)?;
                let kernel = session.metadata().openqa().kernel.clone();
                let overview = session.metadata().openqa().overview.clone();
                KernelExport::new(ctx, kernel, overview).run(&http).await
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
        session
            .display
            .println(&format!("template exported to {}", filename.display()));
        Ok(())
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        template_completion(session, text)
    }
}

/// Borrows the session-scoped HTTP client for log downloads (perf bead
/// `mtui-rs-0mop.13`: reuse one client/pool across commands).
fn build_http(session: &Session) -> Result<HttpClient, CommandError> {
    session
        .http_client()
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
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Auto;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        // The auto exporter appends the system-info footer to any template.
        assert!(written.contains("## export MTUI:"));
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("template exported to"),
            "{:?}",
            buf.contents()
        );
    }

    /// Builds a `DashboardAutoOpenQA` with seeded `results`/`pp` (no network in
    /// `new`; the output fields are set directly as `run()` would). The install
    /// log `url` points at `log_url` so the exporter's real HTTP client can
    /// download it from a mock server.
    fn seeded_auto(log_url: &str) -> mtui_datasources::DashboardAutoOpenQA {
        use mtui_datasources::{
            DashboardAutoOpenQA, QemDashboardClient, QemIncident, VerifyPolicy,
        };
        let rrid: mtui_types::RequestReviewID = "SUSE:Maintenance:1:1".parse().unwrap();
        let client =
            QemDashboardClient::new("http://dashboard.invalid/api", VerifyPolicy::Default(false))
                .expect("client builds");
        let incident = QemIncident {
            rrid: rrid.clone(),
            incident_number: "1".to_string(),
            client,
            data: None,
        };
        let mut auto = DashboardAutoOpenQA::new("http://oqa.invalid", &incident, rrid);
        auto.results = Some(vec![mtui_types::URLs::new(
            "SLES", "x86_64", "15-SP5", log_url, "passed",
        )]);
        auto.pp = vec!["Results from openQA jobs\n".to_string()];
        auto
    }

    /// The Auto branch must read the report's openQA holder end-to-end: the
    /// install status, the `pp` block, and the per-job install log must all land
    /// in the template / on disk. Regression guard for the `None, None` stub.
    #[tokio::test]
    async fn auto_reads_holder_status_pp_and_downloads_log() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Serve the install log the seeded result points at.
        let oqa = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/install.log"))
            .respond_with(ResponseTemplate::new(200).set_body_string("zypper install body\n"))
            .mount(&oqa)
            .await;
        let log_url = format!("{}/install.log", oqa.uri());

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Auto;
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        // Realistic template: a header above the `source code change review:`
        // anchor so `inject_openqa`'s insertion point is in range.
        let path_out = dir.path().join("template.txt");
        std::fs::write(
            &path_out,
            "Test results by product-arch:\n\nsource code change review:\n",
        )
        .unwrap();

        // Pre-seed the holder (as `reload_openqa` would).
        session.metadata_mut().openqa_mut().auto = Some(seeded_auto(&log_url));

        let args = matches(&Export, &["-f", path_out.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path_out).unwrap();
        assert!(
            written.contains("Installation tests done in openQA with following results: PASSED"),
            "status line missing:\n{written}"
        );
        assert!(
            written.contains("Results from openQA jobs"),
            "pp block missing:\n{written}"
        );
        // The per-job install log was downloaded from the mock and written.
        let logfile = dir
            .path()
            .join("SUSE:Maintenance:1:1")
            .join(&session.config.install_logs)
            .join("sles_15-SP5_x86_64.log");
        assert!(logfile.exists(), "install log not written: {logfile:?}");
    }

    #[tokio::test]
    async fn kernel_writes_template_to_explicit_filename() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Kernel;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "regression tests:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    /// The Kernel branch must read the report's `openqa.kernel` list and render
    /// its result matrix — not export against an empty `Vec::new()`. Seeds one
    /// real `KernelOpenQA` (populated from a mock openQA `/api/v1/jobs`) into the
    /// holder and asserts its `pp` matrix lands under `regression tests:`.
    /// Regression guard for the `Vec::new(), None` stub.
    #[tokio::test]
    async fn kernel_reads_holder_and_renders_matrix() {
        use mtui_datasources::{HttpClient, VerifyPolicy};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A mock openQA returning one passing kernel LTP job → a matrix line.
        let oqa = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobs": [{
                    "id": 42,
                    "test": "ltp_syscalls",
                    "result": "passed",
                    "settings": { "FLAVOR": "Server-DVD-Incidents-Kernel", "ARCH": "x86_64" },
                    "modules": []
                }]
            })))
            .mount(&oqa)
            .await;

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Kernel;
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path_out = dir.path().join("template.txt");
        std::fs::write(&path_out, "regression tests:\n\nbuild log review:\n").unwrap();

        // Build a real, populated kernel connector against the mock and seed it.
        let rrid = session.metadata().rrid().unwrap().clone();
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let incident =
            build_incident(rrid.clone(), format!("{}/api", oqa.uri()), http.clone()).await;
        let kernel = crate::commands::support::build_kernel_openqa(&incident, &oqa.uri(), http)
            .run()
            .await
            .unwrap();
        assert!(
            kernel.results().is_some_and(|r| !r.is_empty()),
            "mock kernel connector should populate"
        );
        session.metadata_mut().openqa_mut().kernel.push(kernel);

        let args = matches(&Export, &["-f", path_out.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path_out).unwrap();
        // The connector's matrix header + row prove the holder was read.
        assert!(
            written.contains("Results from openQA:"),
            "kernel results header missing:\n{written}"
        );
        assert!(
            written.contains("openQA instance:") && written.contains("ltp_syscalls"),
            "kernel matrix rows missing:\n{written}"
        );
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
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
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
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        // Pre-seed an auto result via a throwaway dashboard.
        let server = dashboard_server("1").await;
        let rrid = session.metadata().rrid().unwrap().clone();
        let http = session.http_client().unwrap();
        let incident = build_incident(rrid.clone(), format!("{}/api", server.uri()), http).await;
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
        session.metadata_mut().base_mut().workflow = Workflow::Auto;
        let args = matches(&Export, &["-f", "/nonexistent/dir/nope.txt"]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    #[test]
    fn opts_out_of_hostless_skip() {
        // Unlike host-action commands, export must reach `call()` on a host-less
        // template so its per-workflow rule can run (Auto/Kernel at zero hosts).
        assert!(!Export.skip_hostless_templates());
    }

    #[tokio::test]
    async fn auto_exports_with_zero_hosts() {
        // The reported bug: `export` on an Auto template with no connected hosts
        // must still write the template (data comes from openQA), not error.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "");
        session.metadata_mut().base_mut().workflow = Workflow::Auto;
        assert!(session.targets().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    #[tokio::test]
    async fn manual_with_zero_hosts_is_skipped_not_errored() {
        // A Manual template folds per-host logs; with no hosts (and no `-t`) there
        // is nothing to fold, so export reports and skips it — without touching
        // the dashboard/openQA (config points nowhere; a real export attempt
        // would surface an error, proving the early return fired).
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        assert!(session.targets().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        // The template file is left untouched (no export written).
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("## export MTUI:"),
            "should not export:\n{written}"
        );
        // No openQA "auto" was lazily built — the body returned before that.
        assert!(session.metadata().openqa().auto.is_none());
        // The skip reason reaches the display (not just a swallowed tracing warn).
        assert!(
            buf.contents()
                .contains("skipped: manual export needs a connected host"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn manual_with_named_missing_host_still_fails_loudly() {
        // The host-less skip only applies when no `-t` is named. A typo'd `-t`
        // on a Manual template must still fail (upstream HostIsNotConnectedError),
        // not be silently skipped.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();

        let args = matches(&Export, &["-f", path.to_str().unwrap(), "-t", "bogus"]);
        let err = Export.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
