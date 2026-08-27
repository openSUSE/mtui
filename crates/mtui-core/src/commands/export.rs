//! The `export` command (writes the gathered update data to the template).

use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::HttpClient;
use mtui_testreport::{
    AutoExport, DenyOverwrite, ExportContext, FileList, KernelExport, ManualExport, ManualHost,
};
use mtui_types::Workflow;
use mtui_types::package::VersionCheck;

use super::support::{
    add_hosts_arg, build_auto_openqa, build_incident, named_hosts, require_update, select_names,
    template_completion,
};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Exports the gathered update data to the testing template.
///
/// Picks the exporter by the report's [`Workflow`] and writes the pre/post
/// package versions and update log into the template (or `filename`). Requires a
/// loaded report.
///
/// ## openQA enrichment (Manual)
///
/// `Manual` folds openQA results in via the report's holder (`metadata.openqa`):
/// an absent "auto" result is lazily built and run from the QEM Dashboard, then
/// the connected-host results and any `openqa_overview` payload go into
/// [`ManualExport`]. `Auto`/`Kernel` render their full local template.
///
/// A `Manual` export refuses to write at all when *no* selected host has
/// recorded package versions — the signal that this session never ran `update`
/// (#526); `--allow-unverified` writes the scaffold anyway.
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

    /// Opts out of the driver's host-less skip: `Auto`/`Kernel` source from
    /// openQA, so `export --all-templates` must still write them at zero hosts.
    /// The `Manual` rule (which *does* need hosts) lives in
    /// [`call`](Self::call).
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
                Arg::new("allow_unverified")
                    .long("allow-unverified")
                    .action(ArgAction::SetTrue)
                    .help(
                        "write the unverified scaffold even when no selected host has \
                         recorded package versions",
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
        let allow_unverified = args.get_flag("allow_unverified");

        // Nothing to fold, so report and skip rather than write an empty export.
        // A typo'd `-t` still fails loudly below via `select_names`.
        if workflow == Workflow::Manual && !named_hosts(args) && session.targets().is_empty() {
            session
                .display
                .println("skipped: manual export needs a connected host");
            return Ok(());
        }

        let filename: PathBuf = match args.get_one::<String>("filename") {
            Some(f) => PathBuf::from(f),
            None => session
                .metadata()
                .base()
                .path
                .clone()
                .ok_or_else(|| CommandError::Other("no report path to export to".to_owned()))?,
        };

        let (manual_results, manual_overview) = if workflow == Workflow::Manual {
            if session.metadata().openqa().auto.is_none() {
                let http = build_http(session)?;
                let dashboard_api = session.config.qem_dashboard_api.clone();
                let openqa_instance = session.config.openqa_instance.clone();
                let incident = build_incident(
                    rrid.clone(),
                    dashboard_api,
                    http,
                    session.metadata().update_source(),
                )
                .await;
                let mut auto = build_auto_openqa(
                    openqa_instance,
                    &incident,
                    rrid.clone(),
                    session.config.max_parallel as usize,
                );
                // A failed fetch folds to "no results" so the export still
                // renders the rest of the report rather than aborting.
                if let Err(e) = auto.run().await {
                    tracing::warn!(error = %e, "QEM Dashboard fetch failed during export; continuing without auto results");
                }
                session.metadata_mut().openqa_mut().auto = Some(auto);
            }
            let hosts = select_names(session.targets(), args, false)
                .map_err(|e| CommandError::Other(e.to_string()))?;
            let results = manual_hosts(session, &hosts);
            let unverified: Vec<&str> = results
                .iter()
                .filter(|h| is_unverified(h))
                .map(|h| h.hostname.as_str())
                .collect();
            // #526: *every* selected host unverified is the "this session never
            // ran `update`" signal — refuse before any write, since a
            // plausible-but-verdictless testreport is worse than none. A
            // partially verified fleet (a host added after the update) still
            // writes with the per-host warning below.
            if !unverified.is_empty() && unverified.len() == results.len() && !allow_unverified {
                return Err(CommandError::Other(format!(
                    "no package version data recorded for any selected host ({}); run \
                     `update` in this session before exporting, or pass \
                     --allow-unverified to write the unverified scaffold",
                    unverified.join(", ")
                )));
            }
            // #396/#437: a host with no recorded package data — or seeded
            // packages the version query never answered for — keeps the
            // scaffold's unverified lines. Say so where the operator/MCP client
            // sees it, not only in the log.
            for host in &unverified {
                session.display.println(&format!(
                    "WARNING: no package version data recorded for {host}; its install \
                     result was left unverified"
                ));
            }
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

/// Borrows the session-scoped HTTP client, so one pool serves every command.
fn build_http(session: &Session) -> Result<HttpClient, CommandError> {
    session
        .http_client()
        .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))
}

/// Whether the exporter has nothing to verify this host's install result
/// against: not one tracked package the update flow checked on either side
/// (#396/#437) — vacuously true, and deliberately so, for a host with no
/// tracked packages at all. Either side alone can carry the verdict, so both
/// are required: a standalone `downgrade` rotates a checked `after` in with
/// `before` still `NotChecked`. `current` is not consulted — `add_host` fills
/// it on connect, so it would make a session that never ran `update` look
/// verified.
fn is_unverified(host: &ManualHost) -> bool {
    host.packages.iter().all(|p| {
        *p.before_check() == VersionCheck::NotChecked
            && *p.after_check() == VersionCheck::NotChecked
    })
}

/// Builds the [`ManualHost`] views of the named connected targets, so the
/// exporter never reads the live `Target`s directly.
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
    use crate::commands::testkit::{Buffer, empty_session, matches, session_with_hosts};
    use wiremock::MockServer;

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
        assert!(written.contains("## export MTUI:"));
        // A success line reaches the display so the MCP result is never empty.
        assert!(
            buf.contents().contains("template exported to"),
            "{:?}",
            buf.contents()
        );
    }

    /// A `DashboardAutoOpenQA` with `results`/`pp` set directly as `run()`
    /// would, so no network is touched, and an install-log `url` of `log_url` for
    /// the exporter's real HTTP client to download.
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
            source: mtui_types::UpdateSource::Obs,
            client,
            data: None,
        };
        let mut auto = DashboardAutoOpenQA::new("http://oqa.invalid", &incident, rrid, 1);
        auto.results = Some(vec![mtui_types::URLs::new(
            "SLES", "x86_64", "15-SP5", log_url, "passed",
        )]);
        auto.pp = vec!["Results from openQA jobs\n".to_string()];
        auto
    }

    /// The Auto branch must read the holder end-to-end: install status, `pp`
    /// block and per-job install log all land in the template / on disk. Guards
    /// against a regression to the `None, None` stub.
    #[tokio::test]
    async fn auto_reads_holder_status_pp_and_downloads_log() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

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
        // A header above the `source code change review:` anchor keeps
        // `inject_openqa`'s insertion point in range.
        let path_out = dir.path().join("template.txt");
        std::fs::write(
            &path_out,
            "Test results by product-arch:\n\nsource code change review:\n",
        )
        .unwrap();

        // As `reload_openqa` would.
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
    /// its matrix, not export against an empty `Vec::new()`. Guards against a
    /// regression to the `Vec::new(), None` stub.
    #[tokio::test]
    async fn kernel_reads_holder_and_renders_matrix() {
        use mtui_datasources::{HttpClient, VerifyPolicy};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // One passing kernel LTP job → one matrix line.
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

        let rrid = session.metadata().rrid().unwrap().clone();
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let openqa_transport = HttpClient::openqa_transport(VerifyPolicy::Default(false)).unwrap();
        let incident = build_incident(
            rrid.clone(),
            format!("{}/api", oqa.uri()),
            http.clone(),
            session.metadata().update_source(),
        )
        .await;
        let kernel =
            crate::commands::support::build_kernel_openqa(&incident, &oqa.uri(), openqa_transport)
                .unwrap()
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
        // The matrix header + row prove the holder was read.
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
        // #526: without recorded versions the export now refuses; this test is
        // about the openQA fold, so give it a verified host.
        record_versions(&mut session, "h1");
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        let dir = tempfile::tempdir().unwrap();
        // Keeps the per-host install logs the manual exporter writes out of the
        // working tree.
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        assert!(session.metadata().openqa().auto.is_none());
        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        assert!(session.metadata().openqa().auto.is_some());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"));
    }

    /// A manual-workflow session wired to `dashboard_server`, `n` hosts and a
    /// scaffold template on disk. Returns the template path (and keeps `dir`
    /// alive for the caller).
    async fn manual_export_fixture(
        hosts: &[&str],
    ) -> (Session, Buffer, tempfile::TempDir, PathBuf, MockServer) {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", hosts, "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        // The product-arch anchor is what lets the exporter create a per-host
        // block, so the rendered verdict lines are assertable.
        std::fs::write(
            &path,
            "Test results by product-arch:\n\nsource code change review:\n",
        )
        .unwrap();
        (session, buf, dir, path, server)
    }

    /// Seeds `host`'s tracked packages with a recorded before/after pair, as a
    /// completed `update` in this session would.
    fn record_versions(session: &mut Session, host: &str) {
        let t = session.targets_mut().get_mut(host).expect("host present");
        let mut pkg = mtui_types::package::Package::new("bash");
        pkg.set_before(Some("5.1-1")).unwrap();
        pkg.set_after(Some("5.1-2")).unwrap();
        t.set_packages(vec![pkg]);
    }

    /// #526: no host has package data at all — the whole selection is
    /// unverified, so `export` must refuse and leave the template untouched,
    /// naming *every* unverified host (the issue plan's wording).
    /// Mutations caught: dropping the refusal (back to warn-and-write) makes the
    /// `Err` assertion fail; writing first and erroring after makes the
    /// byte-identity assertion fail; naming only the first host drops `h2`.
    #[tokio::test]
    async fn manual_errors_on_unverified_hosts_without_writing() {
        let (mut session, _buf, _dir, path, _server) = manual_export_fixture(&["h1", "h2"]).await;
        let before = std::fs::read(&path).unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        let err = Export.call(&mut session, &args).await.unwrap_err();

        let CommandError::Other(msg) = err else {
            panic!("expected Other");
        };
        assert!(msg.contains("h1"), "{msg}");
        assert!(
            msg.contains("h2"),
            "every unverified host must be named: {msg}"
        );
        assert!(msg.contains("update"), "{msg}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "no partial write may reach the template"
        );
    }

    /// #437 + #526: a host seeded with packages the version query never answered
    /// for is unverified too — `packages.is_empty()` alone missed it, reachable
    /// when a host dies between seed and check.
    #[tokio::test]
    async fn manual_errors_on_seeded_but_unchecked_hosts_without_writing() {
        let (mut session, _buf, _dir, path, _server) = manual_export_fixture(&["h1"]).await;
        for t in session.targets_mut().targets_mut() {
            t.set_packages(vec![mtui_types::package::Package::new("bash")]);
        }
        let before = std::fs::read(&path).unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        let err = Export.call(&mut session, &args).await.unwrap_err();

        let CommandError::Other(msg) = err else {
            panic!("expected Other");
        };
        assert!(msg.contains("h1"), "{msg}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "no partial write");
    }

    /// #526: the predicate is **every** selected host, not **any**. One verified
    /// host plus one unverified one still writes, warning only about the latter.
    /// Mutation caught: widening the guard to `!unverified.is_empty()` turns this
    /// into an `Err`.
    #[tokio::test]
    async fn manual_partially_verified_group_still_writes_with_warning() {
        let (mut session, buf, _dir, path, _server) = manual_export_fixture(&["h1", "h2"]).await;
        record_versions(&mut session, "h1");
        // h2 keeps an empty package list: unverified.

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.expect("must write");

        let out = buf.contents();
        assert!(
            out.contains("WARNING: no package version data recorded for h2"),
            "{out}"
        );
        assert!(
            !out.contains("recorded for h1"),
            "the verified host must not be warned about: {out}"
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"), "{written}");
    }

    /// #526: `is_unverified` is per-host, not per-package — one checked package
    /// verifies the host. `bash` is unchecked on both sides; `bash-doc` carries
    /// only an `after` (the shape a standalone `downgrade` leaves behind, since
    /// it rotates `after <- current` over a never-checked `before`).
    /// Mutations caught: `all` -> `any` (the bare package would refuse the whole
    /// export) and dropping either check-side conjunct (`bash-doc` then reads as
    /// unchecked, so the host does too).
    #[tokio::test]
    async fn manual_mixed_checked_and_unchecked_packages_is_verified() {
        let (mut session, buf, _dir, path, _server) = manual_export_fixture(&["h1"]).await;
        let t = session.targets_mut().get_mut("h1").expect("host present");
        let mut after_only = mtui_types::package::Package::new("bash-doc");
        after_only.set_after(Some("5.1-1")).unwrap();
        t.set_packages(vec![mtui_types::package::Package::new("bash"), after_only]);

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.expect("must write");

        assert!(
            !buf.contents().contains("WARNING: no package version data"),
            "{}",
            buf.contents()
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"), "{written}");
    }

    /// #526: the guard's `!unverified.is_empty()` term. `-t all` opts out of the
    /// host-less skip above, so a zero-host selection reaches the guard with
    /// `unverified.len() == results.len()` trivially `0 == 0`.
    /// Mutation caught: dropping the term refuses with an empty host list.
    #[tokio::test]
    async fn manual_zero_selected_hosts_does_not_refuse() {
        let (mut session, _buf, _dir, path, _server) = manual_export_fixture(&[]).await;

        let args = matches(&Export, &["-f", path.to_str().unwrap(), "-t", "all"]);
        Export.call(&mut session, &args).await.expect("must write");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("## export MTUI:"), "{written}");
    }

    /// #526: `--allow-unverified` is the deliberate opt-out — the scaffold is
    /// written, carries the `not checked` lines, and its verdict placeholder
    /// stays unflipped (pins the `manual.rs` rendering the opt-out relies on).
    #[tokio::test]
    async fn manual_allow_unverified_writes_unflipped_scaffold() {
        let (mut session, buf, _dir, path, _server) = manual_export_fixture(&["h1"]).await;
        for t in session.targets_mut().targets_mut() {
            t.set_packages(vec![mtui_types::package::Package::new("bash")]);
        }

        let args = matches(
            &Export,
            &["-f", "--allow-unverified", path.to_str().unwrap()],
        );
        Export.call(&mut session, &args).await.expect("must write");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("package bash: not checked (no version data recorded)"),
            "{written}"
        );
        assert!(
            written.contains("=> PASSED/FAILED"),
            "verdict must stay undecided:\n{written}"
        );
        assert!(
            !written.contains("=> PASSED\n") && !written.contains("=> FAILED\n"),
            "verdict must not be flipped:\n{written}"
        );
        assert!(
            buf.contents()
                .contains("WARNING: no package version data recorded for h1"),
            "{}",
            buf.contents()
        );
    }

    /// Negative control killing the warn-unconditionally mutant.
    #[tokio::test]
    async fn manual_does_not_warn_when_package_data_recorded() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        for t in session.targets_mut().targets_mut() {
            let mut pkg = mtui_types::package::Package::new("bash");
            pkg.set_before(Some("5.1-1")).unwrap();
            t.set_packages(vec![pkg]);
        }
        let server = dashboard_server("1").await;
        session.config.qem_dashboard_api = format!("{}/api", server.uri());
        session.config.openqa_instance = server.uri();
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        assert!(
            !buf.contents().contains("WARNING: no package version data"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn manual_reuses_existing_openqa_auto() {
        // An existing "auto" result must not be rebuilt.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        record_versions(&mut session, "h1");
        let dir = tempfile::tempdir().unwrap();
        session.config.template_dir = dir.path().to_path_buf();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let server = dashboard_server("1").await;
        let rrid = session.metadata().rrid().unwrap().clone();
        let http = session.http_client().unwrap();
        let incident = build_incident(
            rrid.clone(),
            format!("{}/api", server.uri()),
            http,
            session.metadata().update_source(),
        )
        .await;
        let max_parallel = session.config.max_parallel as usize;
        session.metadata_mut().openqa_mut().auto = Some(build_auto_openqa(
            server.uri(),
            &incident,
            rrid,
            max_parallel,
        ));

        // A rebuild would still succeed (errors are folded away), so the
        // assertion is that the pre-seeded result survives.
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
        // It must reach `call()` on a host-less template so its per-workflow
        // rule can run.
        assert!(!Export.skip_hostless_templates());
    }

    #[tokio::test]
    async fn auto_exports_with_zero_hosts() {
        // The data comes from openQA, so zero hosts must still write, not error.
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
        // Nothing to fold, so it reports and skips without touching the
        // dashboard — whose config points nowhere, so a real attempt would error
        // and prove the early return did not fire.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &[], "");
        session.metadata_mut().base_mut().workflow = Workflow::Manual;
        assert!(session.targets().is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.txt");
        std::fs::write(&path, "source code change review:\n").unwrap();

        let args = matches(&Export, &["-f", path.to_str().unwrap()]);
        Export.call(&mut session, &args).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("## export MTUI:"),
            "should not export:\n{written}"
        );
        // Nothing was lazily built: the body returned before that.
        assert!(session.metadata().openqa().auto.is_none());
        // The skip reason reaches the display, not just a tracing warn.
        assert!(
            buf.contents()
                .contains("skipped: manual export needs a connected host"),
            "{:?}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn manual_with_named_missing_host_still_fails_loudly() {
        // The host-less skip only applies when no `-t` is named.
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
