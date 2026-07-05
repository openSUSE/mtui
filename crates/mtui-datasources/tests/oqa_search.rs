//! Integration + golden-fixture tests for the openQA / QAM Dashboard overview
//! search, ported from upstream `tests/test_oqa_search_connector.py`.
//!
//! The HTTP-facing tests use `wiremock` (upstream used `responses`); the
//! heuristic tests replay the vendored `.log` / `.matches` fixture pairs. The
//! `.matches` files are the byte-for-byte parity signal for the `TESTSUITE_*`
//! constants the connector copies verbatim from upstream — any drift fails here.

use std::path::{Path, PathBuf};

use mtui_datasources::oqa_search::search::{
    aggregated_updates, build_checks, extract_test_results, get_incident_info, incident_jobs,
    single_incidents, summarize_test_results,
};
use mtui_datasources::{HttpClient, VerifyPolicy};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client() -> HttpClient {
    HttpClient::new(VerifyPolicy::Default(true)).expect("client builds")
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oqa_search")
}

// A job-group JSON object matching upstream's `_job_group` helper.
fn job_group(id: i64, name: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "name": name, "template": "tpl"})
}

/// Mount the `/api/v1/job_groups` endpoint returning the given groups.
async fn mount_job_groups(server: &MockServer, groups: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path("/api/v1/job_groups"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(groups)))
        .mount(server)
        .await;
}

// --- single_incidents ---

#[tokio::test]
async fn single_incidents_passed() {
    let server = MockServer::start().await;
    mount_job_groups(
        &server,
        vec![
            job_group(490, "SLE 15 SP5 Core Incidents"),
            job_group(521, "SLE 12 SP4 TERADATA Core Incidents"),
        ],
    )
    .await;
    // Both running and failed overview queries return empty -> PASSED.
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let rows = single_incidents(
        &client(),
        ":12358:bash",
        &["15-SP5".to_string()],
        &server.uri(),
    )
    .await;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].version, "15-SP5");
    assert_eq!(rows[0].status, "passed");
    assert!(
        rows[0]
            .url
            .starts_with(&format!("{}/tests/overview", server.uri()))
    );
    assert!(rows[0].url.contains("groupid=490"));
}

#[tokio::test]
async fn single_incidents_failed_counts_jobs() {
    let server = MockServer::start().await;
    mount_job_groups(&server, vec![job_group(490, "SLE 15 SP5 Core Incidents")]).await;

    // query_version_status hits the overview endpoint twice: running then failed.
    // wiremock does not preserve registration order, so distinguish by the
    // state-specific query params the connector adds.
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .and(query_param("state", "running"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .and(query_param("result", "failed"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([{"id": 1}, {"id": 2}, {"id": 3}])),
        )
        .mount(&server)
        .await;

    let rows = single_incidents(
        &client(),
        ":12358:bash",
        &["15-SP5".to_string()],
        &server.uri(),
    )
    .await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].failed_count, 3);
}

#[tokio::test]
async fn single_incidents_unknown_version_records_note() {
    let server = MockServer::start().await;
    mount_job_groups(&server, vec![job_group(490, "SLE 15 SP5 Core Incidents")]).await;

    let rows = single_incidents(
        &client(),
        ":12358:bash",
        &["99-SP99".to_string()],
        &server.uri(),
    )
    .await;
    assert_eq!(rows[0].status, "failed");
    assert!(rows[0].note.contains("99-SP99"));
}

#[tokio::test]
async fn single_incidents_teradata_uses_base_version_in_url() {
    let server = MockServer::start().await;
    mount_job_groups(
        &server,
        vec![job_group(106, "SLE 12 SP3 TERADATA Core Incidents")],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let rows = single_incidents(
        &client(),
        ":12358:bash",
        &["12-SP3-TERADATA".to_string()],
        &server.uri(),
    )
    .await;

    assert_eq!(rows[0].version, "12-SP3-TERADATA");
    // URL uses the *base* version, not the TERADATA-suffixed one.
    assert!(rows[0].url.contains("version=12-SP3&"));
    assert!(!rows[0].url.contains("TERADATA"));
}

// --- aggregated_updates ---

#[tokio::test]
async fn aggregated_updates_skips_excluded_versions() {
    let server = MockServer::start().await;
    mount_job_groups(
        &server,
        vec![job_group(367, "Core Maintenance Updates 15-SP5")],
    )
    .await;

    let out = aggregated_updates(
        &client(),
        "12358",
        &["15-SP4-TERADATA".to_string(), "16.0".to_string()],
        5,
        &["core".to_string()],
        &server.uri(),
    )
    .await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn aggregated_updates_finds_matching_build() {
    let server = MockServer::start().await;
    mount_job_groups(
        &server,
        vec![job_group(367, "Core Maintenance Updates 15-SP5")],
    )
    .await;

    // The "all" per-day query (no result/state filter) returns one job. Give it
    // the lowest priority so the more-specific running/failed mocks below win
    // when the connector adds those params (wiremock: lower number = higher
    // priority; a plain mock defaults to 5).
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 999}])))
        .with_priority(10)
        .mount(&server)
        .await;
    // Job-issues lookup for job 999 -> includes 12358 (match).
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "job": {"settings": {"INCIDENT_TEST_ISSUES": "12358,12359"}}
        })))
        .mount(&server)
        .await;
    // query_version_status: running + failed both empty -> PASSED. These carry
    // the state/result params, so at default priority (5) they out-rank the
    // priority-10 "all" mock above.
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .and(query_param("state", "running"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .and(query_param("result", "failed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let out = aggregated_updates(
        &client(),
        "12358",
        &["15-SP5".to_string()],
        5,
        &["core".to_string()],
        &server.uri(),
    )
    .await;

    assert_eq!(out.len(), 1);
    let group = &out[0];
    assert_eq!(group.group, "core");
    assert_eq!(group.versions.len(), 1);
    assert_eq!(group.versions[0].version, "15-SP5");
    assert_eq!(group.versions[0].status, "passed");
}

#[tokio::test]
async fn aggregated_updates_missing_after_window() {
    let server = MockServer::start().await;
    mount_job_groups(
        &server,
        vec![job_group(367, "Core Maintenance Updates 15-SP5")],
    )
    .await;
    // Every per-day query returns empty -> exhaust the window.
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let out = aggregated_updates(
        &client(),
        "12358",
        &["15-SP5".to_string()],
        3,
        &["core".to_string()],
        &server.uri(),
    )
    .await;
    let row = &out[0].versions[0];
    assert_eq!(row.status, "missing");
    assert!(row.note.contains("in the last 3 days"));
}

// --- build_checks ---

const HTML_INDEX: &str = r#"
<html><body>
<a href="bash.SUSE_SLE-15-SP5_Update.x86_64.log">log1</a>
<a href="bash.SUSE_SLE-15-SP5_Update.aarch64.log">log2</a>
<a href="other-package.log">unrelated</a>
<a href="README.txt">no-log</a>
</body></html>
"#;

const LOG_SHORT: &str =
    "\n[   12s] === 5 tests passed ===\n[   13s] some other line\n[   14s] 100% tests passed\n";

fn log_long() -> String {
    [
        "[   12s] === run start ===",
        "[   13s] 5 tests passed",
        "[   14s] 6 tests passed",
        "[   15s] 7 tests passed",
        "[   16s] 8 tests passed",
        "[   17s] 9 tests passed",
        "[   18s] === run end ===",
    ]
    .join("\n")
}

const BUILD_CHECKS_PATH: &str = "/testreports/SUSE:Maintenance:12358:199773/build_checks";

#[tokio::test]
async fn build_checks_filters_logs_by_package_and_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(BUILD_CHECKS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(HTML_INDEX))
        .mount(&server)
        .await;
    for arch in ["x86_64", "aarch64"] {
        Mock::given(method("GET"))
            .and(path(format!(
                "{BUILD_CHECKS_PATH}/bash.SUSE_SLE-15-SP5_Update.{arch}.log"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOG_SHORT))
            .mount(&server)
            .await;
    }

    let out = build_checks(
        &client(),
        "Maintenance",
        "12358",
        199773,
        &["bash".to_string()],
        &server.uri(),
        None,
    )
    .await;

    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|e| e.summary.is_empty()));
    assert!(out.iter().all(|e| !e.matches.is_empty()));
}

#[tokio::test]
async fn build_checks_folds_long_match_lists() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(BUILD_CHECKS_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"<a href="bash.x86_64.log">x</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{BUILD_CHECKS_PATH}/bash.x86_64.log")))
        .respond_with(ResponseTemplate::new(200).set_body_string(log_long()))
        .mount(&server)
        .await;

    let out = build_checks(
        &client(),
        "Maintenance",
        "12358",
        199773,
        &["bash".to_string()],
        &server.uri(),
        None,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert!(!out[0].summary.is_empty());
    assert_eq!(out[0].matches.len(), 2);
}

#[tokio::test]
async fn build_checks_index_404_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(BUILD_CHECKS_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let out = build_checks(
        &client(),
        "Maintenance",
        "12358",
        199773,
        &["bash".to_string()],
        &server.uri(),
        None,
    )
    .await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn build_checks_filters_multiple_packages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(BUILD_CHECKS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(HTML_INDEX))
        .mount(&server)
        .await;
    for arch in ["x86_64", "aarch64"] {
        Mock::given(method("GET"))
            .and(path(format!(
                "{BUILD_CHECKS_PATH}/bash.SUSE_SLE-15-SP5_Update.{arch}.log"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOG_SHORT))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("{BUILD_CHECKS_PATH}/other-package.log")))
        .respond_with(ResponseTemplate::new(200).set_body_string(LOG_SHORT))
        .mount(&server)
        .await;

    let out = build_checks(
        &client(),
        "Maintenance",
        "12358",
        199773,
        &["bash".to_string(), "other-package".to_string()],
        &server.uri(),
        None,
    )
    .await;
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn build_checks_matches_flavored_python_package_to_source_log() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(BUILD_CHECKS_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<a href="python-ecdsa.x86_64.log">x</a>"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{BUILD_CHECKS_PATH}/python-ecdsa.x86_64.log")))
        .respond_with(ResponseTemplate::new(200).set_body_string(LOG_SHORT))
        .mount(&server)
        .await;

    let out = build_checks(
        &client(),
        "Maintenance",
        "12358",
        199773,
        &["python313-ecdsa".to_string()],
        &server.uri(),
        None,
    )
    .await;
    assert_eq!(out.len(), 1);
    assert!(out[0].url.ends_with("python-ecdsa.x86_64.log"));
}

// --- get_incident_info ---

#[tokio::test]
async fn get_incident_info_returns_build_and_versions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"settings": {"BUILD": ":12358:bash", "DISTRI": "sle"}, "version": "15-SP5", "flavor": "Server-DVD-Incidents"},
            {"settings": {"BUILD": ":12358:bash", "DISTRI": "sle"}, "version": "15-SP4", "flavor": "Server-DVD-Incidents"},
            {"settings": {"BUILD": ":12358:bash", "DISTRI": "sle"}, "version": "12-SP3", "flavor": "Server-TERADATA"}
        ])))
        .mount(&server)
        .await;

    let (build, versions) = get_incident_info(&client(), &server.uri(), "12358")
        .await
        .expect("ok");
    assert_eq!(build, ":12358:bash");
    let versions = versions.expect("some versions");
    assert!(versions.contains(&"12-SP3-TERADATA".to_string()));
    assert!(versions.contains(&"15-SP4".to_string()));
    assert!(versions.contains(&"15-SP5".to_string()));
}

#[tokio::test]
async fn get_incident_info_no_builds_falls_back_to_package_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"packages": ["bash"]})),
        )
        .mount(&server)
        .await;

    let (build, versions) = get_incident_info(&client(), &server.uri(), "12358")
        .await
        .expect("ok");
    assert_eq!(build, ":12358:bash");
    assert!(versions.is_none());
}

// --- incident_jobs ---

#[tokio::test]
async fn incident_jobs_drops_obsoleted_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobs": [
                {"id": 1, "test": "fips_smoke", "result": "passed", "settings": {"ARCH": "s390x"}},
                {"id": 2, "test": "ha_2nodes", "result": "failed", "settings": {"ARCH": "s390x"}},
                {"id": 3, "test": "old_run", "result": "obsoleted", "settings": {"ARCH": "x86_64"}}
            ]
        })))
        .mount(&server)
        .await;

    let rows = incident_jobs(&client(), ":git:5137:libica", &server.uri(), false)
        .await
        .expect("ok");
    let results: Vec<&str> = rows.iter().map(|r| r.result.as_str()).collect();
    assert_eq!(results, vec!["failed", "passed"]);
    let failed = rows.iter().find(|r| r.result == "failed").unwrap();
    assert_eq!(failed.test, "ha_2nodes");
    assert_eq!(failed.arch, "s390x");
    assert_eq!(failed.url, format!("{}/t2", server.uri()));
}

#[tokio::test]
async fn incident_jobs_include_obsoleted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobs": [{"id": 3, "test": "x", "result": "obsoleted", "settings": {"ARCH": "x86_64"}}]
        })))
        .mount(&server)
        .await;
    let rows = incident_jobs(&client(), ":b", &server.uri(), true)
        .await
        .expect("ok");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].result, "obsoleted");
}

#[tokio::test]
async fn incident_jobs_empty_build_makes_no_request() {
    // A falsy build short-circuits with no HTTP call (no server needed).
    let rows = incident_jobs(&client(), "", "https://openqa.invalid", false)
        .await
        .expect("ok");
    assert!(rows.is_empty());
}

// --- Golden-fixture heuristic tests (parity with the vendored .matches) ---

/// Pair each `.log` with its sibling arch-named `.matches` file.
fn fixture_pairs(package: &str) -> Vec<(PathBuf, PathBuf)> {
    let pkg_dir = fixtures_dir().join(package);
    let mut logs: Vec<PathBuf> = std::fs::read_dir(&pkg_dir)
        .expect("fixture dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("log"))
        .collect();
    logs.sort();
    logs.into_iter()
        .map(|log| {
            // "...x86_64.log" -> stem "...x86_64" -> arch after the last '.'.
            let stem = log.file_stem().unwrap().to_str().unwrap();
            let arch = stem.rsplit('.').next().unwrap();
            let matches = pkg_dir.join(format!("{arch}.matches"));
            assert!(matches.exists(), "missing matches fixture for {log:?}");
            (log, matches)
        })
        .collect()
}

#[test]
fn extract_test_results_real_logs() {
    for package in ["iniparser", "rust"] {
        for (log_path, matches_path) in fixture_pairs(package) {
            let log_text = std::fs::read_to_string(&log_path).expect("read log");
            let expected: Vec<String> = std::fs::read_to_string(&matches_path)
                .expect("read matches")
                .lines()
                .map(str::to_string)
                .collect();
            let actual = extract_test_results(&log_text, None);
            assert_eq!(
                actual,
                expected,
                "heuristic drift for {package} / {:?}",
                log_path.file_name().unwrap()
            );
        }
    }
}

#[test]
fn extract_test_results_rust_folds_via_summarize() {
    let log_path = fixtures_dir().join("rust/rust1.95.SUSE_SLE-15-SP3_Update:test.aarch64.log");
    let log_text = std::fs::read_to_string(&log_path).expect("read log");
    let matches = extract_test_results(&log_text, None);
    assert!(
        matches.len() > 4,
        "expected the aarch64 fixture to produce >4 matches"
    );
    let summary = summarize_test_results(&matches);
    assert!(summary.contains("more results"));
    assert!(!summary.contains("0 passed"));
    assert!(summary.contains("0 failed"));
}
