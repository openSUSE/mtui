//! Sub-bead A (mtui-rs-0pe.1): `TestReport::read` + `make_testreport`.
//!
//! Ports the parse-and-populate slice of upstream `TestReport.read`
//! (`_open_and_parse` → `_parse_json` → `_enrich_issue_titles` →
//! `_update_repos_parse`) and the `UpdateID.make_testreport` factory
//! (`tr_factory` + `_checkout` + workflow/autoconnect selection).
//!
//! All tests run offline: `make_testreport` reads a template pre-placed on disk
//! (so the read-succeeds-first path is taken and no `svn` is spawned), and the
//! load-failure path points at an empty `template_dir` so the internal `svn`
//! checkout fails fast and the factory falls back to a [`NullReport`].

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_testreport::{ObsReport, TestReport, UpdateKind, make_testreport};
use mtui_types::{UpdateID, Workflow};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RRID: &str = "SUSE:Maintenance:24993:275518";

/// An SLFO RRID — `tr_factory` routes it to `SlReport`, whose `check_hash`
/// performs the real Gitea comparison the load-time verification exercises.
const SLFO_RRID: &str = "SUSE:SLFO:1.2:4413";

/// The metadata envelope the checkout fixtures embed (a trimmed real report).
const METADATA_JSON: &str = r#"{
    "bugs": ["12345"],
    "jira": ["SLE-22357"],
    "category": "recommended",
    "packager": "slemke@suse.com",
    "rating": "low",
    "repository": "http://download.suse.de/ibs/SUSE:/Maintenance:/24993/",
    "rrid": "SUSE:Maintenance:24993:275518",
    "packages": {
        "15-SP3": ["sle-module-python2-release = 15.3-150300.59.4.1"]
    },
    "testplatform": [
        "base=sles(major=15,minor=sp3);arch=[x86_64];addon=python2(major=15,minor=sp3)"
    ]
}"#;

/// A `log` template body: two reference-host lines plus a bug title line the
/// `hosts` parser (`ReducedMetadataParser`) should pick up.
const LOG_TEMPLATE: &str = "\
Testreport for SUSE:Maintenance:24993:275518

  x86_64 (reference host: refhost-a.example.com)
  s390x (reference host: refhost-b.example.com)

Bug 12345 (\"broken thing\"):
";

/// `patchinfo.xml` carrying the real title for jira id SLE-22357 (the envelope
/// left it as the placeholder).
const PATCHINFO_XML: &str = r#"<patchinfo>
  <issue tracker="jsc" id="SLE-22357">Real Jira Title</issue>
  <issue tracker="bnc" id="99999">Unrelated Bug</issue>
</patchinfo>"#;

/// Builds a checkout directory `<root>/<rrid>/` with `log`, `metadata.json`, and
/// `project.xml` (so the OBS `update_repos_parser` resolves), plus an optional
/// `patchinfo.xml`. Returns the path to the `log` file.
fn make_checkout(root: &Path, with_patchinfo: bool) -> PathBuf {
    let dir = root.join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();
    std::fs::write(dir.join("metadata.json"), METADATA_JSON).unwrap();
    // OBS `update_repos_parser` reads `project.xml` from `report_wd()`.
    std::fs::write(
        dir.join("project.xml"),
        concat!(
            "<project>\n",
            "  <repository name=\"SLE-15-x86_64\">\n",
            "    <path repository=\"update\" project=\"SUSE:SLE-15:Update\"/>\n",
            "    <releasetarget project=\"SLE-Product-SLES:15:x86_64\"/>\n",
            "  </repository>\n",
            "</project>\n"
        ),
    )
    .unwrap();
    if with_patchinfo {
        std::fs::write(dir.join("patchinfo.xml"), PATCHINFO_XML).unwrap();
    }
    dir.join("log")
}

fn cfg(template_dir: PathBuf) -> Config {
    let mut c = Config::default();
    c.template_dir = template_dir;
    c
}

/// Points a config's QEM-dashboard + openQA URLs at a mock `server` so a
/// `make_testreport(-a)` load resolves openQA offline instead of hitting the
/// production instances baked into `Config::default()`.
fn point_dashboard(config: &mut Config, server: &MockServer) {
    config.qem_dashboard_api = format!("{}/api", server.uri());
    config.openqa_instance = server.uri();
    config.openqa_instance_baremetal = server.uri();
}

/// Mounts the three QEM-dashboard endpoints the auto path touches for
/// `incident_number`, each returning an empty-but-valid body. With no incident
/// settings there are no install jobs, so `DashboardAutoOpenQA` yields
/// `results = None` — the auto→manual downgrade trigger.
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
    // One incident setting whose jobs contain a passing install job.
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

/// `read` populates the base from the `log` (hosts parser) and `metadata.json`
/// (JSON parser), and sets `path`.
#[test]
fn read_populates_metadata_and_hosts() {
    let tmp = tempfile::tempdir().unwrap();
    let log = make_checkout(tmp.path(), false);

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    report.read(&log).expect("read should succeed");

    let base = report.base();
    // JSON envelope fields.
    assert_eq!(base.rrid.as_ref().unwrap().to_string(), RRID);
    assert_eq!(base.packager, "slemke@suse.com");
    assert_eq!(base.category, "recommended");
    assert_eq!(base.testplatforms.len(), 1);
    // Hosts parser picked up both reference hosts from the log.
    assert!(base.hostnames.contains("refhost-a.example.com"));
    assert!(base.hostnames.contains("refhost-b.example.com"));
    // Bug 12345 is listed in the JSON envelope, so after `_parse_json` the JSON
    // placeholder wins over the log's `Bug N ("title"):` line (upstream runs the
    // hosts parser first, then the JSON parser, which re-seeds the ids). Without
    // a patchinfo entry for 12345, the placeholder survives.
    assert_eq!(
        base.bugs.get("12345").map(String::as_str),
        Some("Description not available")
    );
    // `path` is recorded so `report_wd()` / update-repo parsing can resolve.
    assert_eq!(base.path.as_deref(), Some(log.as_path()));
    // OBS update-repo map was derived from project.xml during read.
    assert!(
        !base.update_repos.is_empty(),
        "update_repos should be parsed"
    );
}

/// `patchinfo.xml` overlays the real jira title onto the id the envelope carried
/// (leaving the id set authoritative — the unrelated bug 99999 is not added).
#[test]
fn read_enriches_issue_titles_from_patchinfo() {
    let tmp = tempfile::tempdir().unwrap();
    let log = make_checkout(tmp.path(), true);

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    report.read(&log).unwrap();

    let base = report.base();
    // The envelope's jira id got its real title from patchinfo.
    assert_eq!(
        base.jira.get("SLE-22357").map(String::as_str),
        Some("Real Jira Title")
    );
    // patchinfo's unrelated id 99999 was NOT introduced.
    assert!(!base.bugs.contains_key("99999"));
    assert!(!base.jira.contains_key("99999"));
}

/// A missing `log` file surfaces as an ENOENT template error (so the checkout
/// seam knows to attempt a fresh checkout).
#[test]
fn read_missing_log_is_not_found() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join(RRID).join("log");

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&log).unwrap_err();
    match err {
        ReadError::Template(e) => assert!(e.is_not_found()),
        other => panic!("expected Template(ENOENT), got {other:?}"),
    }
}

/// A present `log` with a missing `metadata.json` is `MetadataMissing`.
#[test]
fn read_missing_metadata_errors() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&dir.join("log")).unwrap_err();
    assert!(matches!(err, ReadError::MetadataMissing));
}

/// A present-but-invalid `metadata.json` is `MetadataInvalid`.
#[test]
fn read_invalid_metadata_errors() {
    use mtui_testreport::ReadError;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("log"), LOG_TEMPLATE).unwrap();
    std::fs::write(dir.join("metadata.json"), "{ not json").unwrap();

    let mut report = ObsReport::new(cfg(tmp.path().to_path_buf()));
    let err = report.read(&dir.join("log")).unwrap_err();
    assert!(matches!(err, ReadError::MetadataInvalid));
}

/// `make_testreport` with an on-disk template loads it, selects the OBS report
/// (Maintenance kind), and — when the dashboard reports **no install jobs** —
/// downgrades the AUTO workflow to MANUAL and (autoconnect=true) marks the
/// report autoconnect-pending (upstream connects only on the manual-downgrade
/// path).
#[tokio::test]
async fn make_testreport_auto_no_install_jobs_downgrades_to_manual() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let server = MockServer::start().await;
    mount_dashboard_no_results(&server, "24993").await;
    let mut config = cfg(tmp.path().to_path_buf());
    point_dashboard(&mut config, &server);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert_eq!(report.id(), RRID);
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

/// When the dashboard reports passing install jobs, the AUTO workflow is kept
/// and — matching upstream — the auto happy-path does **not** autoconnect.
#[tokio::test]
async fn make_testreport_auto_with_install_jobs_stays_auto_no_connect() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let server = MockServer::start().await;
    mount_dashboard_with_install(&server, "24993").await;
    let mut config = cfg(tmp.path().to_path_buf());
    point_dashboard(&mut config, &server);

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert_eq!(report.id(), RRID);
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

/// The kernel kind (`-k`) starts the KERNEL workflow and does **not** autoconnect
/// even when `autoconnect=true` (matching upstream's `autoconnect=False` default
/// for `KernelOBSUpdateID.make_testreport`).
#[tokio::test]
async fn make_testreport_kernel_does_not_autoconnect() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(
        &update,
        cfg(tmp.path().to_path_buf()),
        UpdateKind::Kernel,
        true,
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow(), Workflow::Kernel);
    assert!(
        !report.base().autoconnect_pending,
        "kernel -k must not autoconnect"
    );
}

/// Even on the manual-downgrade path, an explicit `autoconnect=false` (e.g.
/// `--sut` at startup) suppresses the deferred connect.
#[tokio::test]
async fn make_testreport_auto_respects_explicit_no_autoconnect() {
    let tmp = tempfile::tempdir().unwrap();
    make_checkout(tmp.path(), false);
    let update = UpdateID::parse(RRID).unwrap();

    let server = MockServer::start().await;
    mount_dashboard_no_results(&server, "24993").await;
    let mut config = cfg(tmp.path().to_path_buf());
    point_dashboard(&mut config, &server);

    let report = make_testreport(&update, config, UpdateKind::Auto, false, false, None).await;

    // No install jobs → downgraded to manual, but autoconnect=false suppresses
    // the deferred connect.
    assert_eq!(report.workflow(), Workflow::Manual);
    assert!(!report.base().autoconnect_pending);
}

/// Regression (spinner invisible during `update`): `make_testreport` reconciles
/// the loaded report's targets group to the session mode at **load time**, so a
/// REPL load (`is_repl = true`) yields an interactive group — the fan-out
/// spinner / serial-barrier prompt seam — while a headless load stays quiet.
/// The report group is default-built headless; this is the single set-once site.
#[tokio::test]
async fn make_testreport_sets_targets_is_repl_from_session_mode() {
    let update = UpdateID::parse(RRID).unwrap();

    // REPL load → interactive targets group.
    let tmp_repl = tempfile::tempdir().unwrap();
    make_checkout(tmp_repl.path(), false);
    let server = MockServer::start().await;
    mount_dashboard_no_results(&server, "24993").await;
    let mut cfg_repl = cfg(tmp_repl.path().to_path_buf());
    point_dashboard(&mut cfg_repl, &server);
    let repl = make_testreport(&update, cfg_repl, UpdateKind::Auto, false, true, None).await;
    assert!(
        repl.base().targets.is_repl(),
        "REPL load must yield an is_repl targets group"
    );

    // Headless load (MCP) → non-interactive targets group.
    let tmp_head = tempfile::tempdir().unwrap();
    make_checkout(tmp_head.path(), false);
    let server2 = MockServer::start().await;
    mount_dashboard_no_results(&server2, "24993").await;
    let mut cfg_head = cfg(tmp_head.path().to_path_buf());
    point_dashboard(&mut cfg_head, &server2);
    let head = make_testreport(&update, cfg_head, UpdateKind::Auto, false, false, None).await;
    assert!(
        !head.base().targets.is_repl(),
        "headless load must keep a non-interactive targets group"
    );
}

/// When neither the template is on disk nor a checkout can succeed, the factory
/// falls back to a `NullReport` (upstream returns `NullTestReport`) — never an
/// error. `svn_path` is pointed at a bare local `file://` path so `svn co` fails
/// immediately and offline (no network / SSH).
#[tokio::test]
async fn make_testreport_falls_back_to_null_on_load_failure() {
    let tmp = tempfile::tempdir().unwrap();
    // No checkout placed; force the internal `svn co` to fail fast offline by
    // aiming it at a nonexistent local file:// repository.
    let mut config = cfg(tmp.path().to_path_buf());
    config.svn_path = format!("file://{}/no-such-svn-repo", tmp.path().display());
    let update = UpdateID::parse(RRID).unwrap();

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert!(
        !report.is_loaded(),
        "unloaded report should be the null object"
    );
    assert_eq!(report.id(), "");
    // The null report carries *why* the load failed (svn checkout), so the
    // caller can surface it instead of a bare "could not load".
    let reason = report
        .base()
        .load_error
        .as_deref()
        .expect("null report should carry a load_error");
    assert!(
        reason.contains("svn checkout"),
        "load_error should name the svn checkout failure: {reason}"
    );
}

// --- Gitea token + hash verification on load (SLFO / `SlReport`) ------------
//
// Upstream runs `check_hash` at the tail of `TestReport.read` inside
// `UpdateID._checkout`; mtui runs it in `make_testreport` (async seam). These
// exercise the three non-`Ok` branches (the interactive TeReGen regenerate path
// is covered in Phase C) plus the happy path.

/// Places an SLFO checkout on disk whose `metadata.json` carries a Gitea PR API
/// URL (pointed at `gitea_pr_api`) and a recorded commit hash (`giteacohash`).
/// Returns nothing; the caller drives `make_testreport` against the template dir.
fn make_slfo_checkout(root: &Path, gitea_pr_api: &str, commit_hash: &str) {
    let dir = root.join(SLFO_RRID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("log"),
        "Testreport for SUSE:SLFO:1.2:4413\n\n  x86_64 (reference host: refhost-a.example.com)\n",
    )
    .unwrap();
    let metadata = format!(
        r#"{{
            "category": "recommended",
            "packager": "someone@suse.com",
            "rating": "low",
            "rrid": "{SLFO_RRID}",
            "gitea_pr_api": "{gitea_pr_api}",
            "gitea_commit_hash": "{commit_hash}",
            "packages": {{}},
            "testplatform": []
        }}"#
    );
    std::fs::write(dir.join("metadata.json"), metadata).unwrap();
}

/// Mounts a Gitea PR GET returning `{ "head": { "sha": <sha> } }` — what
/// `Gitea::get_hash` reads.
async fn mount_pr_head_sha(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "head": { "sha": sha } })),
        )
        .mount(server)
        .await;
}

/// A matching hash loads the SLFO report normally (workflow set from the auto
/// enrichment) — no regression to the pre-check behaviour. The dashboard is
/// mocked with passing install jobs on a *separate* server so the load keeps the
/// AUTO workflow (the gitea server uses a catch-all GET matcher, which must not
/// swallow the dashboard requests).
#[tokio::test]
async fn make_testreport_slfo_hash_match_loads() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "deadbeef").await;

    let dashboard = MockServer::start().await;
    mount_dashboard_with_install(&dashboard, "4413").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", server.uri()), "deadbeef");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = server.uri();
    point_dashboard(&mut config, &dashboard);
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert!(report.is_loaded(), "matching hash should load the report");
    assert_eq!(report.id(), SLFO_RRID);
    assert_eq!(report.workflow(), Workflow::Auto);
    // Auto happy-path (install jobs present) keeps AUTO and does not autoconnect.
    assert!(!report.base().autoconnect_pending);
}

/// A missing Gitea token abandons the load (null report). The `Gitea` client
/// refuses to build without a token, so no network call is made.
#[tokio::test]
async fn make_testreport_slfo_missing_token_yields_null() {
    let tmp = tempfile::tempdir().unwrap();
    // A PR API URL is present in metadata, but the config has no token (default).
    make_slfo_checkout(tmp.path(), "http://gitea.invalid/pulls/1", "deadbeef");

    let config = cfg(tmp.path().to_path_buf());
    assert!(config.gitea_token.is_empty(), "precondition: no token");
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert!(
        !report.is_loaded(),
        "a missing Gitea token must abandon the load"
    );
    assert_eq!(report.id(), "");
    let reason = report
        .base()
        .load_error
        .as_deref()
        .expect("null report should carry a load_error");
    assert!(
        reason.contains("token is not configured"),
        "load_error should name the missing token: {reason}"
    );
}

/// A stale template hash (differs from the Gitea PR head) abandons the load
/// (null report) in the non-interactive path — matching upstream's
/// `InvalidGiteaHashError` degradation before the TeReGen prompt (Phase C).
#[tokio::test]
async fn make_testreport_slfo_hash_mismatch_yields_null() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "freshsha").await;

    let tmp = tempfile::tempdir().unwrap();
    // Stored hash "stalesha" differs from the PR head "freshsha".
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", server.uri()), "stalesha");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = server.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    let report = make_testreport(&update, config, UpdateKind::Auto, true, false, None).await;

    assert!(
        !report.is_loaded(),
        "a stale template hash must abandon the load (non-interactive)"
    );
    assert_eq!(report.id(), "");
    let reason = report
        .base()
        .load_error
        .as_deref()
        .expect("null report should carry a load_error");
    assert!(
        reason.contains("hash mismatch"),
        "load_error should name the hash mismatch: {reason}"
    );
}

// --- Interactive stale-hash handling (upstream `_checkout` prompt sequence) --
//
// A scripted `Prompter` answers each `[y/n]` question by matching a substring of
// its prompt text, so a test can drive the regenerate / force-continue / delete
// branches deterministically without a real terminal.

/// Builds a `Prompter` whose answer to each prompt is chosen by the first
/// `(needle, answer)` whose `needle` the prompt text contains. Unmatched prompts
/// answer with an empty string (i.e. the prompt's default).
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

/// Interactive, stale hash, decline TeReGen, then **force continue**: the stale
/// report is kept (loaded), matching upstream's "Template is loaded, but hash
/// differs".
#[tokio::test]
async fn make_testreport_slfo_mismatch_force_continue_keeps_stale() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "freshsha").await;

    // The forced-continue report loads, so it reaches the auto enrichment; keep
    // that offline by pointing the dashboard at a mock (no install jobs → the
    // workflow downgrades to manual, which this test does not assert).
    let dashboard = MockServer::start().await;
    mount_dashboard_no_results(&dashboard, "4413").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", server.uri()), "stalesha");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = server.uri();
    point_dashboard(&mut config, &dashboard);
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? no.  Force continue? yes.
    let prompter = scripted_prompter(&[("Regenerate", "n"), ("Force continue", "y")]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(
        report.is_loaded(),
        "force-continue keeps the stale report loaded"
    );
    assert_eq!(report.id(), SLFO_RRID);
}

/// Interactive, stale hash, decline TeReGen and decline force-continue, then
/// **confirm delete**: the checkout dir is removed and the load is abandoned.
#[tokio::test]
async fn make_testreport_slfo_mismatch_decline_deletes_checkout() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "freshsha").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", server.uri()), "stalesha");
    let checkout_dir = tmp.path().join(SLFO_RRID);
    assert!(checkout_dir.exists(), "precondition: checkout present");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = server.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? no.  Force continue? no.  Delete? yes.
    let prompter = scripted_prompter(&[
        ("Regenerate", "n"),
        ("Force continue", "n"),
        ("Delete", "y"),
    ]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(!report.is_loaded(), "declining both abandons the load");
    assert_eq!(report.id(), "");
    assert!(
        !checkout_dir.exists(),
        "confirming delete must remove the stale checkout"
    );
}

/// Interactive, stale hash, decline TeReGen and force-continue, and **decline
/// delete**: the load is abandoned but the checkout is left in place.
#[tokio::test]
async fn make_testreport_slfo_mismatch_decline_keeps_checkout_when_delete_declined() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "freshsha").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", server.uri()), "stalesha");
    let checkout_dir = tmp.path().join(SLFO_RRID);

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = server.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? no.  Force continue? no.  Delete? no (default is yes, but an
    // explicit "n" declines).
    let prompter = scripted_prompter(&[
        ("Regenerate", "n"),
        ("Force continue", "n"),
        ("Delete", "n"),
    ]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(!report.is_loaded(), "declining both abandons the load");
    assert!(
        checkout_dir.exists(),
        "declining delete must leave the checkout in place"
    );
}

/// Interactive, stale hash, **accept regenerate** but TeReGen refuses the job:
/// regeneration fails, so the flow falls back to the manual prompts; declining
/// both (and delete) abandons the load. Exercises the TeReGen-refused path.
#[tokio::test]
async fn make_testreport_slfo_regenerate_refused_falls_back_to_manual() {
    use wiremock::matchers::{method, path};

    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;

    // TeReGen refuses the regeneration with an `{"error": …}` body.
    let teregen = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/reports/{SLFO_RRID}/regenerate")))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(serde_json::json!({ "error": "template was edited" })),
        )
        .mount(&teregen)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", gitea.uri()), "stalesha");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    config.teregen_api = teregen.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? yes (but it's refused) → manual: Force continue? no. Delete? no.
    let prompter = scripted_prompter(&[
        ("Regenerate", "y"),
        ("Force continue", "n"),
        ("Delete", "n"),
    ]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(
        !report.is_loaded(),
        "a refused regeneration falls back to manual, which was declined"
    );
}

/// Mounts a TeReGen server that accepts the regenerate POST (returning a job id)
/// and reports the given terminal `minion_state` on the status poll.
async fn mount_teregen(server: &MockServer, minion_state: &str) {
    use wiremock::matchers::method;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "job": 42 })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "minion_state": minion_state, "minion_error": "boom" }),
        ))
        .mount(server)
        .await;
}

/// Interactive, stale hash, accept regenerate; TeReGen enqueues the job but it
/// does **not finish** (`minion_state=failed`). Regeneration fails → manual
/// fallback (declined) → null. Exercises the `!outcome.ok` branch and the
/// stale-checkout removal after a job is accepted.
#[tokio::test]
async fn make_testreport_slfo_regenerate_job_unfinished_removes_checkout() {
    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;

    let teregen = MockServer::start().await;
    mount_teregen(&teregen, "failed").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", gitea.uri()), "stalesha");
    let checkout_dir = tmp.path().join(SLFO_RRID);

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    config.teregen_api = teregen.uri();
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? yes (job accepted, doesn't finish) → manual declined.
    let prompter = scripted_prompter(&[
        ("Regenerate", "y"),
        ("Force continue", "n"),
        ("Delete", "n"),
    ]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(!report.is_loaded(), "an unfinished job abandons the load");
    // A job was accepted, so the stale checkout is dropped before the failure.
    assert!(
        !checkout_dir.exists(),
        "an accepted-but-unfinished job removes the stale checkout"
    );
}

/// Interactive, stale hash, accept regenerate; TeReGen finishes the job, but the
/// fresh checkout fails offline (bad `svn_path`), so the reload fails and the
/// load is abandoned. Exercises the job-finished path through the fresh
/// checkout/read "Reload after regeneration failed" branch.
#[tokio::test]
async fn make_testreport_slfo_regenerate_finished_but_reload_fails() {
    let gitea = MockServer::start().await;
    mount_pr_head_sha(&gitea, "freshsha").await;

    let teregen = MockServer::start().await;
    mount_teregen(&teregen, "finished").await;

    let tmp = tempfile::tempdir().unwrap();
    make_slfo_checkout(tmp.path(), &format!("{}/pulls/1", gitea.uri()), "stalesha");

    let mut config = cfg(tmp.path().to_path_buf());
    config.gitea_token = "tok".to_owned();
    config.gitea_url = gitea.uri();
    config.teregen_api = teregen.uri();
    // The stale checkout is removed after the job is accepted; the fresh `svn co`
    // then fails fast offline against a nonexistent local repo.
    config.svn_path = format!("file://{}/no-such-svn-repo", tmp.path().display());
    let update = UpdateID::parse(SLFO_RRID).unwrap();

    // Regenerate? yes (finishes) → fresh checkout fails → manual declined.
    let prompter = scripted_prompter(&[
        ("Regenerate", "y"),
        ("Force continue", "n"),
        ("Delete", "n"),
    ]);

    let report = make_testreport(
        &update,
        config,
        UpdateKind::Auto,
        true,
        true,
        Some(&prompter),
    )
    .await;

    assert!(
        !report.is_loaded(),
        "a finished job whose fresh checkout fails abandons the load"
    );
}
