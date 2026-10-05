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
use mtui_testreport::{UpdateKind, make_testreport};
use mtui_types::{UpdateID, Workflow};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
                                   "pull_request": "https://x/pr/1"}},
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

/// Mounts the three QEM-dashboard endpoints the auto path touches, each with an
/// empty-but-valid body. No incident settings means no install jobs, so
/// `DashboardAutoOpenQA` yields `results = None` — the auto→manual trigger.
async fn mount_dashboard_no_results(server: &MockServer, incident_number: &str) {
    for (endpoint, body) in [
        ("incidents", serde_json::json!({})),
        ("incident_settings", serde_json::json!([])),
        ("update_settings", serde_json::json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/{endpoint}/{incident_number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
}

/// Mounts a dashboard with one passing `qam-incidentinstall` job for
/// `incident_number`, so `DashboardAutoOpenQA` resolves `results = Some(..)` and
/// the auto workflow is kept (no downgrade).
async fn mount_dashboard_with_install(server: &MockServer, incident_number: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/api/incidents/{incident_number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/incident_settings/{incident_number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"id": 1, "settings": {"DISTRI": "sle", "VERSION": "15-SP5", "ARCH": "x86_64"}}
        ])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/update_settings/{incident_number}")))
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
        false,
        None,
        false,
    )
    .await;

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
        false,
        None,
        false,
    )
    .await;

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
        false,
        None,
        false,
    )
    .await;

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
        false,
        None,
        false,
    )
    .await;

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
        false,
        None,
        false,
    )
    .await;

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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Kernel,
        false,
        true,
        Some(&prompter),
        false,
    )
    .await;

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
        false,
        None,
        false,
    )
    .await;

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

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, false).await;

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

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, false).await;

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
        false,
        None,
        false,
    )
    .await;

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

    let report =
        make_testreport(&update, config, UpdateKind::Auto, false, false, None, false).await;

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
        true,
        None,
        false,
    )
    .await;
    assert!(
        repl.base().targets.is_repl(),
        "REPL load must yield an is_repl targets group"
    );

    let head = make_testreport(&update, config, UpdateKind::Auto, false, false, None, false).await;
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

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, false).await;

    assert!(report.is_loaded(), "matching hash should load the report");
    assert_eq!(report.id(), SLFO_RRID);
    assert_eq!(report.workflow(), Workflow::Auto);
    assert!(!report.base().autoconnect_pending);
}

/// A missing Gitea token abandons the load: the client refuses to build without
/// one, so no network call is made.
#[tokio::test]
async fn make_testreport_slfo_missing_token_yields_null_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "deadbeef", "deadbeef").await;
    config.gitea_token.clear();

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, false).await;

    assert!(
        !report.is_loaded(),
        "a missing Gitea token must abandon the load"
    );
    assert_eq!(report.id(), "");
    let reason = report.base().load_error.as_deref().expect("a load_error");
    assert!(
        reason.contains("token is not configured"),
        "load_error should name the missing token: {reason}"
    );
}

/// A stale commit (differs from the Gitea PR head) abandons a non-interactive
/// load before any regenerate prompt.
#[tokio::test]
async fn make_testreport_slfo_hash_mismatch_yields_null_document() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, _gitea, _teregen, update) =
        slfo_session(tmp.path().to_path_buf(), "freshsha", "stalesha").await;

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, false).await;

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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
        false,
    )
    .await;

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

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None, true).await;

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
        true,
        Some(&prompter),
        true,
    )
    .await;

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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
        false,
    )
    .await;

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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
        false,
    )
    .await;

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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
        false,
    )
    .await;

    assert!(!report.is_loaded(), "an unfinished job abandons the load");
    assert!(
        rrid_dir.exists(),
        "the scratch directory must survive an accepted regenerate"
    );
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

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
        false,
    )
    .await;

    assert!(
        !report.is_loaded(),
        "a finished job whose reload fails abandons the load"
    );
}
