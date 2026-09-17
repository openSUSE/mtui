//! End-to-end `make_testreport` coverage for the `api-ingest` read path.
//!
//! `lifecycle.rs`'s colocated `ingest_tests` module unit-tests
//! `load_via_document`/`document_fetch_message`/`regenerate_via_teregen`
//! directly; this file drives the same machinery through the public
//! `make_testreport` entry point so its `#[cfg(feature = "api-ingest")]`
//! wiring — including the `HashCheck::Mismatch` -> `handle_stale_hash` ->
//! `regenerate_via_teregen` branch — is exercised too, not just the private
//! helpers it calls.
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

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_testreport::{UpdateKind, make_testreport};
use mtui_types::UpdateID;
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
/// accepted, `GET .../status` immediately `finished`.
async fn mount_v1_regenerate_finished(server: &MockServer, rrid: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/reports/{rrid}/regenerate")))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({"job": 1})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{rrid}/status")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"minion_state": "finished"})),
        )
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

/// Whether `svn`/`svnadmin` are on `PATH`. The real-checkout case below skips
/// cleanly when absent, mirroring `mtui-core::commands::checkout`'s own gate
/// and the CI `test` job's `SVN fixture` step (the only job that installs
/// `subversion` and runs this feature).
fn svn_available() -> bool {
    std::process::Command::new("svn")
        .arg("--version")
        .output()
        .is_ok()
        && std::process::Command::new("svnadmin")
            .arg("--version")
            .output()
            .is_ok()
}

/// Creates a local `file://` SVN repo under `root` with `<rrid>` pre-populated
/// from `files` (name, content pairs), via a real `svnadmin create` + `svn
/// import`. Returns the `svn_path` base (no trailing `/<rrid>`) a real `svn
/// co` can check out from — offline, no network involved.
fn make_svn_repo(root: &Path, rrid: &str, files: &[(&str, &str)]) -> String {
    let repo = root.join("svnrepo");
    assert!(
        std::process::Command::new("svnadmin")
            .args(["create", repo.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    let staging = root.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    for (name, content) in files {
        std::fs::write(staging.join(name), content).unwrap();
    }
    let url = format!("file://{}/{rrid}", repo.display());
    assert!(
        std::process::Command::new("svn")
            .args(["import", staging.to_str().unwrap(), &url, "-m", "init"])
            .status()
            .unwrap()
            .success()
    );
    format!("file://{}", repo.display())
}

/// A `200` document response loads via `apply_document`, the real SVN checkout
/// still runs for the scratch directory (P3-D1), and the SVN parsers
/// (`ReducedMetadataParser`/`JSONParser`) are never consulted: the checked-out
/// `metadata.json` carries a bug id no document in this file ever declares —
/// if a regression routed the load back through `TestReport::read`, that id
/// would appear in `base.bugs`.
#[tokio::test]
async fn make_testreport_ingest_200_applies_document_and_runs_the_checkout() {
    if !svn_available() {
        return;
    }

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
    // A poison `metadata.json`/`log`: only `TestReport::read`'s SVN parsers
    // would ever surface bug "424242" — no document here declares it.
    let svn_path = make_svn_repo(
        tmp.path(),
        MAINT_RRID,
        &[
            ("log", "Testreport for SUSE:Maintenance:1:2\n"),
            ("metadata.json", r#"{"bugs": ["424242"]}"#),
        ],
    );

    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    config.svn_path = svn_path;
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
    let wd = base.report_wd().expect("report_wd resolves");
    assert_eq!(wd, rrid_dir);
    // `report_wd()` itself creates a missing directory, so its `Ok` alone
    // would be true even without a checkout; `.svn` only exists if the real
    // `svn co` ran, and the imported `metadata.json` only exists if it
    // actually pulled the repo's content.
    assert!(
        rrid_dir.join(".svn").is_dir(),
        "the real SVN checkout should have run"
    );
    assert!(rrid_dir.join("metadata.json").exists());

    assert!(
        !base.bugs.contains_key("424242"),
        "the SVN-only bug must not leak into a document-loaded report: {:?}",
        base.bugs
    );
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
/// `regenerate_via_teregen` (the feature-gated reload-via-document branch),
/// which reloads a fresh document and loads it — covering the wiring
/// `lifecycle.rs`'s colocated unit tests exercise only in isolation.
#[tokio::test]
async fn make_testreport_ingest_gitea_mismatch_regenerates_and_reloads_via_document() {
    if !svn_available() {
        return;
    }

    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;
    let gitea_api = format!("{}/pulls/1", gitea.uri());

    let teregen = MockServer::start().await;
    mount_v1_regenerate_finished(&teregen, SLFO_RRID).await;
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
    let svn_path = make_svn_repo(
        tmp.path(),
        SLFO_RRID,
        &[("log", "Testreport for SUSE:SLFO:1.2:7819\n")],
    );

    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api = teregen.uri();
    config.teregen_api_v2 = teregen.uri();
    config.svn_path = svn_path;
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
    if !svn_available() {
        return;
    }

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
    let svn_path = make_svn_repo(
        tmp.path(),
        SLFO_RRID,
        &[("log", "Testreport for SUSE:SLFO:1.2:7819\n")],
    );

    let mut config = cfg(tmp.path().to_path_buf());
    config.teregen_api_v2 = server.uri();
    config.svn_path = svn_path;
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
