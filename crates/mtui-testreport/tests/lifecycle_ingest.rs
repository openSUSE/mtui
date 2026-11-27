//! End-to-end `make_testreport` coverage for the document read path.
//!
//! `lifecycle.rs`'s colocated `ingest_tests` module unit-tests
//! `load_via_document`/`document_fetch_message`/`regenerate_via_teregen`
//! directly; this file drives the same machinery through the public
//! `make_testreport` entry point, including the `HashCheck::Mismatch` ->
//! `handle_stale_hash` -> `regenerate_via_teregen` branch.
//!
//! The 503-past-the-600s-poll-budget status is deliberately not repeated
//! here as a literal end-to-end wait: `document_fetch_message` already pins
//! its exact text (colocated `ingest_tests`), and pairing
//! `#[tokio::test(start_paused = true)]` with a real wiremock socket races
//! the client's own read timeout against the mocked response (verified by
//! hand: the request fails with a spurious read-timeout `Transport` error
//! well before the mocked body is ever read), so a fast, reliable version of
//! that exact scenario isn't practical. The bounded-retry test below proves
//! the same retry loop is reachable from `make_testreport`, at the same ~5s
//! real cost the colocated retry test already pays.

use std::path::PathBuf;

use mtui_config::options::Config;
use mtui_testreport::{LoadCancelled, LoadContext, UpdateKind, make_testreport};
use mtui_types::sanitize::TRUNCATION_MARK;
use mtui_types::{UpdateID, Workflow};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::log_capture::capture_events_async;

/// A maintenance/OBS RRID — `tr_factory` routes it to `ObsReport`, whose
/// `check_hash` is constant `Ok`, so these cases never touch Gitea.
const MAINT_RRID: &str = "SUSE:Maintenance:1:2";

/// An SLFO/Gitea RRID — routes to `SlReport`, whose `check_hash` performs the
/// real Gitea commit comparison the stale-hash/regenerate case needs.
const SLFO_RRID: &str = "SUSE:SLFO:1.2:7819";

fn cfg(template_dir: PathBuf) -> Config {
    let mut c = Config::default();
    c.template_dir = template_dir;
    c
}

/// A minimal, schema-valid v2 document for a maintenance/OBS update.
fn maintenance_document(id: &str) -> String {
    format!(
        r#"{{
            "schema_version": "1.0", "id": "{id}", "kind": "maintenance",
            "workflow": "obs", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {{"testers": [], "reviewer": {{"name": null}}}},
            "update": {{"packager": "p", "source_packages": ["a"], "origin": {{}},
                       "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}]}},
            "install": {{"repository": "http://x/", "targets": [{{
                "product": "n", "version": "v", "arch": "x86_64",
                "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
            }}], "test_platforms": []}},
            "issues": {{}}, "testing": {{}}
        }}"#
    )
}

/// A minimal, schema-valid v2 document for an SLFO/Gitea update, carrying
/// `commit` as `update.origin.commit` — what `SlReport::check_hash` compares
/// against the mocked Gitea PR head.
fn slfo_gitea_document(id: &str, gitea_api: &str, commit: &str) -> String {
    format!(
        r#"{{
            "schema_version": "1.0", "id": "{id}", "kind": "slfo",
            "workflow": "gitea", "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null, "comment": null,
            "people": {{"testers": [], "reviewer": {{"name": null}}}},
            "update": {{"packager": "p", "source_packages": ["a"],
                       "origin": {{"api": "{gitea_api}", "commit": "{commit}",
                                   "pull_request": "https://src.suse.de/products/SLFO/pulls/7819"}},
                       "products": [{{"name": "n", "version": "v", "archs": ["x86_64"]}}]}},
            "install": {{"repository": "http://x/", "targets": [{{
                "product": "n", "version": "v", "arch": "x86_64",
                "repository": "http://x/r", "binaries": {{"a": "1-1.x86_64"}}
            }}], "test_platforms": []}},
            "issues": {{}}, "testing": {{}}
        }}"#
    )
}

/// Mounts a Gitea PR GET returning `{ "head": { "sha": <sha> } }` — what
/// `Gitea::get_hash` reads. Mirrors `tests/lifecycle.rs`'s helper of the
/// same name.
async fn mount_pr_head_sha(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "head": { "sha": sha } })),
        )
        .mount(server)
        .await;
}

/// Mounts the v1 regenerate write path for `rrid`: `POST .../regenerate`
/// accepted, `GET .../status` immediately in `minion_state`.
async fn mount_v1_regenerate(server: &MockServer, rrid: &str, minion_state: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/reports/{rrid}/regenerate")))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{rrid}/status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"minion_state": minion_state, "minion_error": "boom"}),
        ))
        .mount(server)
        .await;
}

/// Serves `document` as `GET /reports/{id}`.
async fn serve_document(server: &MockServer, id: &str, document: String) {
    Mock::given(method("GET"))
        .and(path(format!("/reports/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(document))
        .mount(server)
        .await;
}

/// Points a config's QEM-dashboard + openQA URLs at a mock `server`, so a
/// `-a` load resolves openQA offline instead of hitting production.
fn point_dashboard(config: &mut Config, server: &MockServer) {
    config.qem_dashboard_api = format!("{}/api", server.uri());
    config.openqa_instance = server.uri();
    config.openqa_instance_baremetal = server.uri();
}

/// The `project` and `type` the dashboard requires for the fixture incident:
/// `7819` is the SLFO pull request, every other number a maintenance incident.
fn dashboard_key(incident_number: &str) -> (String, &'static str) {
    if incident_number == "7819" {
        ("products/SLFO".to_owned(), "git")
    } else {
        (format!("SUSE:Maintenance:{incident_number}"), "smelt")
    }
}

/// Mounts the three QEM-dashboard endpoints the auto path touches, each with an
/// empty-but-valid body. No incident settings means no install jobs, so
/// `DashboardAutoOpenQA` yields `results = None` — the auto→manual trigger.
async fn mount_dashboard_no_results(server: &MockServer, incident_number: &str) {
    let (project, kind) = dashboard_key(incident_number);
    for (endpoint, body) in [
        ("incidents", serde_json::json!({})),
        ("incident_settings", serde_json::json!([])),
        ("update_settings", serde_json::json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/{endpoint}/{incident_number}")))
            .and(query_param("project", &project))
            .and(query_param("type", kind))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
}

/// Mounts a dashboard with one passing `qam-incidentinstall` job for
/// `incident_number`, so `DashboardAutoOpenQA` resolves `results = Some(..)` and
/// the auto workflow is kept (no downgrade).
async fn mount_dashboard_with_install(server: &MockServer, incident_number: &str) {
    let (project, kind) = dashboard_key(incident_number);
    Mock::given(method("GET"))
        .and(path(format!("/api/incidents/{incident_number}")))
        .and(query_param("project", &project))
        .and(query_param("type", kind))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/incident_settings/{incident_number}")))
        .and(query_param("project", &project))
        .and(query_param("type", kind))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 1, "settings": {"DISTRI": "sle", "VERSION": "15-SP5", "ARCH": "x86_64"}}
        ])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/update_settings/{incident_number}")))
        .and(query_param("project", &project))
        .and(query_param("type", kind))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/jobs/incident/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"job_id": 42, "name": "qam-incidentinstall-x86_64", "status": "passed"}
        ])))
        .mount(server)
        .await;
}

/// A scripted `Prompter` answering each `[y/n]` question by the first
/// `(needle, answer)` whose `needle` the prompt text contains. Mirrors
/// `tests/lifecycle.rs`'s helper of the same name.
fn scripted_prompter(script: &'static [(&'static str, &'static str)]) -> mtui_hosts::Prompter {
    mtui_hosts::Prompter::new(std::sync::Arc::new(move |text: String| {
        let answer = script
            .iter()
            .find(|(needle, _)| text.contains(needle))
            .map_or(String::new(), |(_, a)| (*a).to_owned());
        Box::pin(async move { Ok(answer) })
            as std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>>
    }))
}

/// An interactive REPL load answering through `prompter`.
fn repl(prompter: &mtui_hosts::Prompter) -> LoadContext<'_> {
    LoadContext {
        is_repl: true,
        prompter: Some(prompter),
        ..LoadContext::default()
    }
}

/// A `200` document response loads via `apply_document`: only the scratch
/// directory is created, holding neither `log` nor `metadata.json`.
#[tokio::test]
async fn make_testreport_ingest_200_applies_document_without_a_checkout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(maintenance_document(MAINT_RRID))
                .insert_header("etag", "\"abc123\""),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(report.is_loaded(), "a 200 document should load the report");
    assert_eq!(report.id(), MAINT_RRID);

    let base = report.base();
    assert_eq!(
        base.document.as_ref().map(|d| d.id.as_str()),
        Some(MAINT_RRID)
    );
    assert_eq!(base.document_etag.as_deref(), Some("\"abc123\""));

    let rrid_dir = tmp.path().join(MAINT_RRID);
    assert_eq!(base.path.as_deref(), Some(rrid_dir.join("log").as_path()));
    assert!(rrid_dir.is_dir());
    assert!(!rrid_dir.join("log").exists());
    assert!(!rrid_dir.join("metadata.json").exists());
}

/// `404` maps to the exact P3-D5 "no document yet" text on a `NullReport`.
#[tokio::test]
async fn make_testreport_ingest_404_yields_null_report_with_p3_d5_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded());
    assert_eq!(
        report.base().load_error.as_deref(),
        Some("no document yet — run `regenerate` first")
    );
}

/// `409` maps to the exact P3-D5 "stale" text.
#[tokio::test]
async fn make_testreport_ingest_409_yields_null_report_with_p3_d5_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded());
    assert_eq!(
        report.base().load_error.as_deref(),
        Some("the server's document is stale — run `regenerate`")
    );
}

/// `400` maps to the exact P3-D5 "rejects this id" text, carrying the
/// server's own detail verbatim.
#[tokio::test]
async fn make_testreport_ingest_400_yields_null_report_with_p3_d5_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "bad rrid"})),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded());
    assert_eq!(
        report.base().load_error.as_deref(),
        Some("the server rejects this id: bad rrid")
    );
}

/// A transient `503` is retried (not refused immediately) even through the
/// public `make_testreport` entry point, not just the colocated
/// `load_via_document` unit test. Real-time bound by one `POLL_INTERVAL`
/// sleep (~5s) — see the module doc for why the full 600s budget isn't
/// exercised literally.
#[tokio::test]
async fn make_testreport_ingest_503_then_404_retries_before_failing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{MAINT_RRID}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded());
    assert_eq!(
        report.base().load_error.as_deref(),
        Some("no document yet — run `regenerate` first"),
        "the retry must have run and then surfaced the status after it"
    );
}

/// A stale Gitea hash on the v2 document drives `handle_stale_hash` ->
/// `regenerate_via_teregen` (the reload-via-document branch),
/// which reloads a fresh document and loads it — covering the wiring
/// `lifecycle.rs`'s colocated unit tests exercise only in isolation.
#[tokio::test]
async fn make_testreport_ingest_gitea_mismatch_regenerates_and_reloads_via_document() {
    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;
    let gitea_api = format!("{}/pulls/1", gitea.uri());

    let teregen = MockServer::start().await;
    mount_v1_regenerate(&teregen, SLFO_RRID, "finished").await;
    // The initial load sees a stale commit; the reload after the regenerate
    // job finishes sees the fresh one.
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(slfo_gitea_document(SLFO_RRID, &gitea_api, "stalesha")),
        )
        .up_to_n_times(1)
        .mount(&teregen)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(slfo_gitea_document(SLFO_RRID, &gitea_api, "freshsha")),
        )
        .mount(&teregen)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let prompter = scripted_prompter(&[("Regenerate", "y")]);

    let report = make_testreport(&update, config, UpdateKind::Kernel, false, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(
        report.is_loaded(),
        "a successful regenerate + document reload should load the fresh report: {:?}",
        report.base().load_error
    );
    assert_eq!(report.base().giteacohash.as_deref(), Some("freshsha"));
}

/// A stale Gitea hash reached non-interactively (no prompter) never offers to
/// regenerate, never force-continues, and never deletes the checkout —
/// `handle_stale_hash`'s manual-fallback tail (declined path) — abandoning the
/// load with the exact P3-D5-adjacent decline text.
#[tokio::test]
async fn make_testreport_ingest_gitea_mismatch_noninteractive_declines_and_yields_null() {
    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;
    let gitea_api = format!("{}/pulls/1", gitea.uri());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(slfo_gitea_document(SLFO_RRID, &gitea_api, "stalesha")),
        )
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded());
    assert_eq!(
        report.base().load_error.as_deref(),
        Some(
            "template hash mismatch (stale checkout); regeneration \
             declined or unavailable"
        )
    );
}

// --- Workflow, autoconnect and stale-hash handling on the document path ---

/// A maintenance RRID whose incident number (`24993`) the dashboard mocks key on.
const DASHBOARD_RRID: &str = "SUSE:Maintenance:24993:275518";

/// A teregen serving the maintenance document for [`DASHBOARD_RRID`], and a
/// config pointing at it with the dashboard mocked by `mount`.
async fn maintenance_session(template_dir: PathBuf) -> (Config, MockServer, MockServer, UpdateID) {
    let teregen = MockServer::start().await;
    serve_document(
        &teregen,
        DASHBOARD_RRID,
        maintenance_document(DASHBOARD_RRID),
    )
    .await;
    let dashboard = MockServer::start().await;
    let mut config = cfg(template_dir);
    config.teregen_api_v2 = teregen.uri();
    point_dashboard(&mut config, &dashboard);
    (
        config,
        teregen,
        dashboard,
        UpdateID::parse(DASHBOARD_RRID).unwrap(),
    )
}

/// With **no install jobs** on the dashboard, AUTO downgrades to MANUAL and
/// (autoconnect=true) the report is marked autoconnect-pending.
#[tokio::test]
async fn make_testreport_auto_no_install_jobs_downgrades_to_manual_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _teregen, dashboard, update) = maintenance_session(tmp.path().to_path_buf()).await;
    mount_dashboard_no_results(&dashboard, "24993").await;

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert_eq!(report.id(), DASHBOARD_RRID);
    assert_eq!(
        report.workflow(),
        Workflow::Manual,
        "no install jobs must switch mode to manual"
    );
    assert!(report.base().openqa.auto.is_some(), "auto result populated");
    assert!(
        report.base().autoconnect_pending,
        "the manual-downgrade path defers a connect when autoconnect=true"
    );
}

/// With passing install jobs the AUTO workflow is kept and the happy path does
/// **not** autoconnect.
#[tokio::test]
async fn make_testreport_auto_with_install_jobs_stays_auto_no_connect_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _teregen, dashboard, update) = maintenance_session(tmp.path().to_path_buf()).await;
    mount_dashboard_with_install(&dashboard, "24993").await;

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert_eq!(
        report.workflow(),
        Workflow::Auto,
        "install jobs present must keep the auto workflow"
    );
    let auto = report.base().openqa.auto.as_ref().expect("auto populated");
    assert!(auto.results.is_some(), "install results resolved");
    assert!(
        !report.base().autoconnect_pending,
        "the auto happy-path must not autoconnect on load"
    );
}

/// The kernel kind starts the KERNEL workflow and never autoconnects, even when
/// `autoconnect=true` and the dashboard would have triggered it for AUTO.
#[tokio::test]
async fn make_testreport_kernel_does_not_autoconnect_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _teregen, dashboard, update) = maintenance_session(tmp.path().to_path_buf()).await;
    mount_dashboard_no_results(&dashboard, "24993").await;

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert_eq!(report.workflow(), Workflow::Kernel);
    assert!(
        !report.base().autoconnect_pending,
        "kernel -k must not autoconnect"
    );
}

/// Even on the manual-downgrade path an explicit `autoconnect=false` (e.g.
/// `--sut` at startup) suppresses the deferred connect.
#[tokio::test]
async fn make_testreport_auto_respects_explicit_no_autoconnect_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _teregen, dashboard, update) = maintenance_session(tmp.path().to_path_buf()).await;
    mount_dashboard_no_results(&dashboard, "24993").await;

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert_eq!(report.workflow(), Workflow::Manual);
    assert!(!report.base().autoconnect_pending);
}

/// The targets group is built headless and `make_testreport` reconciles it to
/// the session mode once: a REPL load yields an interactive group (the fan-out
/// spinner seam), a headless load stays quiet.
#[tokio::test]
async fn make_testreport_sets_targets_is_repl_from_session_mode_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _teregen, dashboard, update) = maintenance_session(tmp.path().to_path_buf()).await;
    mount_dashboard_no_results(&dashboard, "24993").await;

    let repl = make_testreport(
        &update,
        config.clone(),
        UpdateKind::Auto,
        false,
        LoadContext {
            is_repl: true,
            ..LoadContext::default()
        },
    )
    .await
    .expect("not cancelled");
    assert!(
        repl.base().targets.is_repl(),
        "REPL load must yield an is_repl targets group"
    );

    let head = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");
    assert!(
        !head.base().targets.is_repl(),
        "headless load must keep a non-interactive targets group"
    );
}

/// Gitea PR head `head_sha`, a teregen serving `document_commit` as the
/// document's `update.origin.commit`, and a config wired to both.
async fn slfo_session(
    template_dir: PathBuf,
    head_sha: &str,
    document_commit: &str,
) -> (Config, MockServer, MockServer, UpdateID) {
    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, head_sha).await;
    let teregen = MockServer::start().await;
    serve_document(
        &teregen,
        SLFO_RRID,
        slfo_gitea_document(
            SLFO_RRID,
            &format!("{}/pulls/1", gitea.uri()),
            document_commit,
        ),
    )
    .await;
    let mut config = cfg(template_dir);
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    (config, gitea, teregen, UpdateID::parse(SLFO_RRID).unwrap())
}

/// SL Micro / SLFO updates are Gitea pull requests; a `kind: slfo` document
/// served through the OBS workflow is refused at load.
#[tokio::test]
async fn make_testreport_slfo_obs_document_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let teregen = MockServer::start().await;
    serve_document(
        &teregen,
        SLFO_RRID,
        slfo_gitea_document(SLFO_RRID, "http://x/pulls/1", "deadbeef")
            .replace(r#""workflow": "gitea""#, r#""workflow": "obs""#),
    )
    .await;
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(!report.is_loaded(), "slfo + obs must not load");
    let reason = report.base().load_error.as_deref().expect("a load_error");
    assert!(
        reason.contains("Gitea pull requests only"),
        "load_error should say SLFO updates are Gitea-only: {reason}"
    );
}

/// A matching hash loads the SLFO report; the happy path keeps AUTO and does
/// not autoconnect. The dashboard is a separate server from Gitea, whose
/// catch-all GET matcher would otherwise swallow its requests.
#[tokio::test]
async fn make_testreport_slfo_hash_match_loads_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "deadbeef", "deadbeef").await;
    let dashboard = MockServer::start().await;
    mount_dashboard_with_install(&dashboard, "7819").await;
    point_dashboard(&mut config, &dashboard);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(report.is_loaded(), "matching hash should load the report");
    assert_eq!(report.id(), SLFO_RRID);
    assert_eq!(report.workflow(), Workflow::Auto);
    assert!(!report.base().autoconnect_pending);
}

/// A missing Gitea token abandons the load: the client refuses to build without
/// one, so no network call is made. The hint names the file in use, under `~`.
#[tokio::test]
async fn make_testreport_slfo_missing_token_yields_null_document() {
    let tmp = tempfile::tempdir().unwrap();
    for (explicit, named) in [
        (None, "in ~/.config/mtui/mtui.toml"),
        (Some("/home/tester/qa/mtui.toml"), "in ~/qa/mtui.toml"),
        (Some("/srv/qa.toml"), "in /srv/qa.toml"),
    ] {
        let (mut config, _gitea, _teregen, update) =
            slfo_session(tmp.path().to_path_buf(), "deadbeef", "deadbeef").await;
        config.gitea_token.clear();
        config.source.home = Some("/home/tester".into());
        config.source.user_file = Some("/home/tester/.config/mtui/mtui.toml".into());
        config.source.explicit = explicit.map(Into::into);

        let report = make_testreport(
            &update,
            config,
            UpdateKind::Auto,
            true,
            LoadContext::default(),
        )
        .await
        .expect("not cancelled");

        assert!(
            !report.is_loaded(),
            "a missing Gitea token must abandon the load"
        );
        assert_eq!(report.id(), "");
        let reason = report.base().load_error.as_deref().expect("a load_error");
        assert_eq!(
            reason,
            format!(
                "Gitea API token is not configured. Pass -g/--gitea-token, or set \
                 token under [gitea] {named}."
            ),
            "the hint names only what mtui reads"
        );
    }
}

/// A stale commit (differs from the Gitea PR head) abandons a non-interactive
/// load before any regenerate prompt.
#[tokio::test]
async fn make_testreport_slfo_hash_mismatch_yields_null_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");

    assert!(
        !report.is_loaded(),
        "a stale template hash must abandon the load (non-interactive)"
    );
    assert_eq!(report.id(), "");
    let reason = report.base().load_error.as_deref().expect("a load_error");
    assert!(
        reason.contains("hash mismatch"),
        "load_error should name the hash mismatch: {reason}"
    );
}

/// Interactive, stale hash, decline regenerate, then **force continue**: the
/// stale report is kept, with the same `stale_hash_warning` a non-interactive
/// force-continue sets.
#[tokio::test]
async fn make_testreport_slfo_mismatch_force_continue_keeps_stale_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let dashboard = MockServer::start().await;
    mount_dashboard_no_results(&dashboard, "7819").await;
    point_dashboard(&mut config, &dashboard);

    let prompter = scripted_prompter(&[("Regenerate", "n"), ("Force continue", "y")]);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(
        report.is_loaded(),
        "force-continue keeps the stale report loaded"
    );
    assert_eq!(report.id(), SLFO_RRID);
    assert!(
        report.base().stale_hash_warning.is_some(),
        "the interactive force-continue path must set the warning too"
    );
}

/// Non-interactive, no prompter: `force_continue=true` reaches the outcome the
/// REPL's "y" does — the stale report is kept — and never regenerates (the
/// `expect(0)` fails the test if the POST is hit).
#[tokio::test]
async fn make_testreport_slfo_noninteractive_force_continue_keeps_stale_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let dashboard = MockServer::start().await;
    mount_dashboard_no_results(&dashboard, "7819").await;
    point_dashboard(&mut config, &dashboard);
    let regenerate = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&regenerate)
        .await;
    config.teregen_api = regenerate.uri();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext {
            force_continue: true,
            ..LoadContext::default()
        },
    )
    .await
    .expect("not cancelled");

    assert!(
        report.is_loaded(),
        "force_continue=true keeps the stale report loaded without a prompter"
    );
    assert_eq!(report.id(), SLFO_RRID);
}

/// Interactive: `force_continue=true` does not bypass the REPL's own prompt —
/// a scripted decline still abandons the load.
#[tokio::test]
async fn make_testreport_slfo_interactive_force_continue_arg_ignored_when_prompter_present_document()
 {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let prompter = scripted_prompter(&[("Regenerate", "n"), ("Force continue", "n")]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        LoadContext {
            force_continue: true,
            ..repl(&prompter)
        },
    )
    .await
    .expect("not cancelled");

    assert!(
        !report.is_loaded(),
        "an interactive decline must win over force_continue=true"
    );
}

/// Declining regenerate and force-continue abandons the load and keeps the
/// report directory. The scripted prompter answers only those two questions: a
/// further prompt would get an empty answer, so a restored delete prompt
/// (default yes) would remove the directory.
#[tokio::test]
async fn make_testreport_slfo_mismatch_decline_keeps_scratch_dir_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let rrid_dir = tmp.path().join(SLFO_RRID);
    let logs = rrid_dir.join("install_logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("x.log"), "kept").unwrap();
    let prompter = scripted_prompter(&[("Regenerate", "n"), ("Force continue", "n")]);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(!report.is_loaded(), "declining both abandons the load");
    assert_eq!(report.id(), "");
    assert_eq!(std::fs::read_to_string(logs.join("x.log")).unwrap(), "kept");
}

/// Accepting regenerate but teregen refusing the job falls back to the manual
/// prompts; declining those abandons the load.
#[tokio::test]
async fn make_testreport_slfo_regenerate_refused_falls_back_to_manual_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let refusing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(serde_json::json!({ "error": "template was edited" })),
        )
        .mount(&refusing)
        .await;
    config.teregen_api = refusing.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(
        !report.is_loaded(),
        "a refused regeneration falls back to manual, which was declined"
    );
}

/// An accepted regenerate job that does **not finish** falls back to the manual
/// prompts. The report directory only holds scratch
/// files here and is kept.
#[tokio::test]
async fn make_testreport_slfo_regenerate_job_unfinished_keeps_scratch_dir_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let failing = MockServer::start().await;
    mount_v1_regenerate(&failing, SLFO_RRID, "failed").await;
    config.teregen_api = failing.uri();
    let rrid_dir = tmp.path().join(SLFO_RRID);
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(!report.is_loaded(), "an unfinished job abandons the load");
    assert!(
        rrid_dir.exists(),
        "the scratch directory must survive an accepted regenerate"
    );
}

/// A cancel during the stale-template regeneration ends the load as cancelled,
/// without falling back to the force-continue question.
#[tokio::test]
async fn make_testreport_slfo_regenerate_cancelled_ends_the_load_as_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let running = MockServer::start().await;
    mount_v1_regenerate(&running, SLFO_RRID, "active").await;
    config.teregen_api = running.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "y")]);
    let ctx = repl(&prompter);
    ctx.cancel.cancel();

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        make_testreport(&update, config, UpdateKind::Auto, true, ctx),
    )
    .await
    .expect("the cancel must end the job wait");

    assert!(
        matches!(out, Err(LoadCancelled::Regeneration)),
        "{:?}",
        out.err()
    );
}

/// A regenerate request that fails outright (here 500) abandons the load
/// without polling for a job that was never enqueued.
#[tokio::test]
async fn make_testreport_slfo_regenerate_request_failure_abandons_the_load() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({"error": "x"})))
        .expect(1)
        .mount(&failing)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}/status")))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&failing)
        .await;
    config.teregen_api = failing.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);

    let mut report = None;
    let events = capture_events_async(async {
        report = Some(
            make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
                .await
                .expect("not cancelled"),
        );
    })
    .await;

    assert!(!report.expect("ran").is_loaded());
    assert!(
        events.contains("Regeneration request failed: TeReGen answered 500"),
        "{events}"
    );
    assert!(!events.contains("did not finish"), "{events}");
}

/// A regenerate job that finishes but whose document reload fails (here 404)
/// abandons the load.
#[tokio::test]
async fn make_testreport_slfo_regenerate_finished_but_reload_fails_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    // The initial load sees the stale document once; the reload then 404s.
    let teregen = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(slfo_gitea_document(
                SLFO_RRID,
                &format!("{}/pulls/1", gitea.uri()),
                "stalesha",
            )),
        )
        .up_to_n_times(1)
        .mount(&teregen)
        .await;
    mount_v1_regenerate(&teregen, SLFO_RRID, "finished").await;
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
        .await
        .expect("not cancelled");

    assert!(
        !report.is_loaded(),
        "a finished job whose reload fails abandons the load"
    );
}

// --- TeReGen's text reaching the log ---

/// Server text carrying a C1 CSI introducer (with its parameters), a bidi
/// override, a newline that would start a forged record, and a tail long
/// enough to pass the log's cap; [`READABLE`] is the start of what survives
/// sanitizing it.
fn hostile() -> String {
    format!(
        "bad \u{9b}2J\u{202e}reason\nFORGED record{}",
        "x".repeat(600)
    )
}
const READABLE: &str = "bad reason\\nFORGED record";

fn assert_sanitized(events: &str, expected: &str) {
    assert!(events.contains(expected), "{events:?}");
    assert!(!events.contains('\u{9b}'), "{events:?}");
    assert!(!events.contains('\u{202e}'), "{events:?}");
    assert!(events.contains(TRUNCATION_MARK), "{events:?}");
    assert!(!events.contains(&"x".repeat(500)), "{events:?}");
    assert!(
        !events.lines().any(|l| l.starts_with("FORGED")),
        "{events:?}"
    );
}

/// Drives a stale SLFO load into `regenerate_via_teregen` against `v1` (the
/// regenerate API) and returns every log line it emitted.
async fn regenerate_events(v1: &MockServer) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    config.teregen_api = v1.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);
    capture_events_async(async {
        let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
            .await
            .expect("not cancelled");
        assert!(!report.is_loaded());
    })
    .await
}

#[tokio::test]
async fn regeneration_refusal_is_sanitized_in_the_log() {
    let v1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(serde_json::json!({"error": hostile()})),
        )
        .mount(&v1)
        .await;

    let events = regenerate_events(&v1).await;
    assert_sanitized(&events, &format!("Regeneration refused: {READABLE}"));
}

#[tokio::test]
async fn regeneration_request_failure_is_sanitized_in_the_log() {
    let v1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(serde_json::json!({"error": hostile()})),
        )
        .mount(&v1)
        .await;

    let events = regenerate_events(&v1).await;
    assert_sanitized(
        &events,
        &format!(
            "Regeneration request failed: TeReGen answered 500 Internal Server Error: {READABLE}"
        ),
    );
}

#[tokio::test]
async fn regeneration_minion_error_is_sanitized_in_the_log() {
    let v1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
        .mount(&v1)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}/status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"minion_state": "failed", "minion_error": hostile()}),
        ))
        .mount(&v1)
        .await;

    let events = regenerate_events(&v1).await;
    assert_sanitized(
        &events,
        &format!("Regeneration did not finish (state=failed): {READABLE}"),
    );
}

#[tokio::test]
async fn reload_after_regeneration_failure_is_sanitized_in_the_log() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;
    let teregen = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(slfo_gitea_document(
                SLFO_RRID,
                &format!("{}/pulls/1", gitea.uri()),
                "stalesha",
            )),
        )
        .up_to_n_times(1)
        .mount(&teregen)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{SLFO_RRID}")))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": hostile()})),
        )
        .mount(&teregen)
        .await;
    mount_v1_regenerate(&teregen, SLFO_RRID, "finished").await;
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    let prompter = scripted_prompter(&[("Regenerate", "y"), ("Force continue", "n")]);

    let events = capture_events_async(async {
        let report = make_testreport(&update, config, UpdateKind::Auto, true, repl(&prompter))
            .await
            .expect("not cancelled");
        assert!(!report.is_loaded());
    })
    .await;
    assert_sanitized(
        &events,
        &format!("Reload after regeneration failed: the server rejects this id: {READABLE}"),
    );
}

#[tokio::test]
async fn schema_drift_is_sanitized_in_the_log() {
    let teregen = MockServer::start().await;
    serve_document(&teregen, MAINT_RRID, maintenance_document(MAINT_RRID)).await;
    let mut schema: serde_json::Value =
        serde_json::from_str(mtui_types::report_document::SCHEMA_JSON).unwrap();
    schema[hostile()] = serde_json::json!(true);
    Mock::given(method("GET"))
        .and(path("/schema"))
        .respond_with(ResponseTemplate::new(200).set_body_json(schema))
        .mount(&teregen)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = teregen.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let events = capture_events_async(async {
        make_testreport(
            &update,
            config,
            UpdateKind::Kernel,
            false,
            LoadContext::default(),
        )
        .await
        .expect("not cancelled");
    })
    .await;
    assert_sanitized(&events, &format!("drift=/{READABLE}x"));
}

/// Loads an SLFO document whose PR API URL (`update.origin.api`, TeReGen's
/// text) is `api(gitea_uri)`, against a Gitea that fails every request, and
/// returns every log line the load emitted.
async fn gitea_failure_events(api: impl Fn(&str) -> String) -> String {
    let gitea = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&gitea)
        .await;
    let teregen = MockServer::start().await;
    serve_document(
        &teregen,
        SLFO_RRID,
        slfo_gitea_document(SLFO_RRID, &api(&gitea.uri()), "sha"),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    config.teregen_api_v2 = teregen.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    capture_events_async(async {
        let report = make_testreport(
            &update,
            config,
            UpdateKind::Auto,
            false,
            LoadContext::default(),
        )
        .await
        .expect("not cancelled");
        assert!(!report.is_loaded());
    })
    .await
}

/// The failed hash check logs the PR API URL on several lines, at WARN among
/// them; none may carry what TeReGen put in it.
#[tokio::test]
async fn gitea_failure_is_sanitized_in_the_log() {
    // JSON-escaped, so the document carries a real newline.
    let events =
        gitea_failure_events(|uri| format!("{uri}/pulls/1\u{9b}2J\u{202e}\\nFORGED")).await;
    let line = events
        .lines()
        .find(|l| l.contains("TestReport isn't loaded") && l.contains("error="))
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(
        line.contains("/pulls/1FORGED returned status 500"),
        "{line:?}"
    );
    assert!(
        events.contains("failed with status code: 500"),
        "{events:?}"
    );
    assert!(!events.contains(['\u{9b}', '\u{202e}']), "{events:?}");
    assert!(
        !events.lines().any(|l| l.starts_with("FORGED")),
        "{events:?}"
    );
}

/// A PR API URL on another origin is refused before any request, and the
/// refusal names it.
#[tokio::test]
async fn untrusted_gitea_origin_is_sanitized_in_the_log() {
    let events =
        gitea_failure_events(|_| "http://evil.invalid/pulls/1\u{9b}2J\u{202e}".to_owned()).await;
    assert!(
        events
            .contains("Refusing to send Gitea token to untrusted URL http://evil.invalid/pulls/1"),
        "{events:?}"
    );
    assert!(!events.contains(['\u{9b}', '\u{202e}']), "{events:?}");
}

/// The load error is what `load_template` shows on the REPL and over MCP, and
/// a document that fails to parse puts TeReGen's own value in it.
#[tokio::test]
async fn an_unparseable_document_is_sanitized_in_the_load_error() {
    let teregen = MockServer::start().await;
    serve_document(
        &teregen,
        MAINT_RRID,
        maintenance_document(MAINT_RRID).replace(
            r#""kind": "maintenance""#,
            "\"kind\": \"bad\u{9b}2J\u{202e}kind\"",
        ),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = teregen.uri();
    let update = UpdateID::parse(MAINT_RRID).unwrap();

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        LoadContext::default(),
    )
    .await
    .expect("not cancelled");
    let load_error = report
        .base()
        .load_error
        .clone()
        .expect("a failed load names why");
    assert!(load_error.contains("badkind"), "{load_error:?}");
    assert!(
        !load_error.contains(['\u{9b}', '\u{202e}']),
        "{load_error:?}"
    );
}
