//! Integration tests for the QEM Dashboard connector, exercising the public
//! `QemIncident` + `DashboardAutoOpenQA` API end-to-end against a mock server.
//!
//! The pure-helper and async-timeout assertions are colocated unit tests in
//! `src/qem_dashboard/`.

use mtui_datasources::http::{HttpClient, VerifyPolicy};
use mtui_datasources::qem_dashboard::{DashboardAutoOpenQA, QemDashboardClient, QemIncident};
use mtui_types::{RequestReviewID, UpdateSource};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OPENQA_HOST: &str = "https://openqa.example.com";

fn client_for(server: &MockServer) -> QemDashboardClient {
    let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
    QemDashboardClient::with_client(http, format!("{}/api", server.uri()))
}

#[tokio::test]
async fn loads_incident_and_aggregate_jobs() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"number": 12358, "packages": ["bash"], "channels": []})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 7,
            "incident": 12358,
            "version": "15-SP5",
            "flavor": "Server-DVD-Incidents",
            "arch": "x86_64",
            "settings": {"DISTRI": "sle"}
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/jobs/incident/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "job_id": 1001,
            "name": "qam-incidentinstall",
            "job_group": "Maintenance",
            "group_id": 1,
            "status": "passed",
            "distri": "sle",
            "flavor": "Server-DVD-Incidents",
            "arch": "x86_64",
            "version": "15-SP5",
            "build": ":12358:bash",
            "obsolete": false
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 23,
            "incidents": [12358],
            "product": "SLES-15-SP5",
            "arch": "x86_64",
            "build": "20240101-1",
            "repohash": "abc123",
            "settings": {"DISTRI": "sle", "VERSION": "15-SP5"}
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/jobs/update/23"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "job_id": 1002,
            "name": "mau-webserver@64bit",
            "job_group": "Aggregate",
            "group_id": 2,
            "status": "failed",
            "distri": "sle",
            "flavor": "Server-DVD-Updates",
            "arch": "x86_64",
            "version": "15-SP5",
            "build": "20240101-1",
            "obsolete": false
        }])))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident =
        QemIncident::with_client(rrid.clone(), client_for(&server), UpdateSource::Obs).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid, 4);
    dashboard.run().await.unwrap();

    let pp = dashboard.pp.concat();
    assert!(pp.contains("Incident jobs"));
    assert!(pp.contains("Aggregate jobs"));
    assert!(pp.contains("mau-webserver"));

    let results = dashboard
        .results
        .as_ref()
        .expect("install job passed -> results");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].url,
        "https://openqa.example.com/tests/1001/file/update_install-zypper.log"
    );
    assert_eq!(results[0].result, "passed");
    assert!(dashboard.is_present());
}

#[tokio::test]
async fn slfo_1_2_incident_uses_review_id() {
    // An SLFO 1.2 request's incident number is the review id, so every
    // dashboard URL keys on it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"number": 12358, "packages": ["bash"], "channels": []})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:SLFO:1.2:12358".parse().unwrap();
    let incident =
        QemIncident::with_client(rrid.clone(), client_for(&server), UpdateSource::Obs).await;
    assert_eq!(incident.incident_number, "12358");

    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid, 4);
    dashboard.run().await.unwrap();
    // No jobs -> no results, no rendered block.
    assert!(dashboard.pp.is_empty());
    assert!(dashboard.results.is_none());
    assert!(!dashboard.is_present());
}

#[tokio::test]
async fn incident_metadata_shortest_package_name() {
    // Public-API path.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 12358,
            "packages": ["kernel-default", "kernel-ec2"],
            "channels": ["SUSE:SLE-12-SP2:Update"]
        })))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
    assert!(incident.is_present());
    assert_eq!(incident.get_incident_name().as_deref(), Some("kernel-ec2"));
}

#[tokio::test]
async fn oversized_incident_body_folds_to_absent() {
    // A body over MAX_API_BODY is rejected by the bounded read and folded to
    // None, so a hostile response degrades to "incident not present".
    use mtui_datasources::MAX_API_BODY;
    let server = MockServer::start().await;
    let oversized = vec![b'x'; MAX_API_BODY + 1];
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
    assert!(!incident.is_present());
}

#[tokio::test]
async fn run_errors_when_dashboard_unreachable() {
    // Both settings endpoints 500: the dashboard could not be asked at all, so
    // `run` surfaces the failure rather than folding to an empty result.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number": 12358})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident =
        QemIncident::with_client(rrid.clone(), client_for(&server), UpdateSource::Obs).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid, 4);
    assert!(dashboard.run().await.is_err());
}

#[tokio::test]
async fn run_ok_on_empty_success() {
    // Valid but empty settings: a genuinely-empty successful response is Ok with
    // no results, distinct from the unreachable-dashboard error above.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"number": 12358})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident =
        QemIncident::with_client(rrid.clone(), client_for(&server), UpdateSource::Obs).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid, 4);
    dashboard.run().await.unwrap();
    assert!(dashboard.results.is_none());
    assert!(dashboard.pp.is_empty());
}

/// Mount an incident with a passed install job plus a failed job, and an
/// aggregate with one passed job.
async fn mount_mixed_jobs(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 7, "incident": 12358, "version": "15-SP5",
            "flavor": "Server-DVD-Incidents", "arch": "x86_64",
            "settings": {"DISTRI": "sle"}
        }])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/jobs/incident/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"job_id": 1001, "name": "qam-incidentinstall", "status": "passed",
             "distri": "sle", "flavor": "Server-DVD-Incidents", "arch": "x86_64",
             "version": "15-SP5", "build": ":12358:bash", "obsolete": false},
            {"job_id": 1003, "name": "qam-bash", "status": "failed",
             "distri": "sle", "flavor": "Server-DVD-Incidents", "arch": "x86_64",
             "version": "15-SP5", "build": ":12358:bash", "obsolete": false}
        ])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 23, "incidents": [12358], "product": "SLES-15-SP5",
            "arch": "x86_64", "build": "20240101-1", "repohash": "abc123",
            "settings": {"DISTRI": "sle", "VERSION": "15-SP5"}
        }])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/jobs/update/23"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"job_id": 1002, "name": "mau-webserver@64bit", "status": "passed",
             "distri": "sle", "flavor": "Server-DVD-Updates", "arch": "x86_64",
             "version": "15-SP5", "build": "20240101-1", "obsolete": false}
        ])))
        .mount(server)
        .await;
}

async fn dashboard_for(server: &MockServer) -> DashboardAutoOpenQA {
    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident =
        QemIncident::with_client(rrid.clone(), client_for(server), UpdateSource::Obs).await;
    DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid, 4)
}

const EXPECTED_PP: &str = "\
\nResults from openQA jobs:\n\
=========================\n\
\nIncident jobs:\n\
\x20 Summary:\n\
\x20     version: 15-SP5 - flavor: Server-DVD-Incidents - arch: x86_64 -> passed: 1, failed: 1, total: 2\n\
\x20 Failed jobs:\n\
\x20   15-SP5 / Server-DVD-Incidents / x86_64 (1 failed):\n\
\x20     qam-bash  https://openqa.example.com/tests/1003\n\
\nAggregate jobs:\n\
\x20 build: 20240101-1\n\
\x20 Summary:\n\
\x20     product: SLES-15-SP5 - archs: x86_64 -> passed: 1, total: 1 (1 arch)\n\
\x20 All jobs passed.\n\
\n";

#[tokio::test]
async fn run_renders_exact_results_and_pp_for_mixed_jobs() {
    let server = MockServer::start().await;
    mount_mixed_jobs(&server).await;

    let mut dashboard = dashboard_for(&server).await;
    dashboard.run().await.unwrap();

    assert_eq!(
        dashboard.results,
        Some(vec![mtui_types::URLs::new(
            "sle",
            "x86_64",
            "15-SP5",
            "Server-DVD-Incidents",
            "https://openqa.example.com/tests/1001/file/update_install-zypper.log",
            "passed",
        )])
    );
    assert_eq!(dashboard.pp.concat(), EXPECTED_PP);
}

#[tokio::test]
async fn failed_refresh_keeps_previous_outputs() {
    let server = MockServer::start().await;
    mount_mixed_jobs(&server).await;

    let mut dashboard = dashboard_for(&server).await;
    dashboard.run().await.unwrap();
    let results = dashboard.results.clone();
    let pp = dashboard.pp.clone();
    assert!(results.is_some() && !pp.is_empty());

    server.reset().await;
    for p in ["/api/incident_settings/12358", "/api/update_settings/12358"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    }
    assert!(dashboard.run().await.is_err());
    assert_eq!(dashboard.results, results);
    assert_eq!(dashboard.pp, pp);
}

/// Two incident settings (x86_64 then aarch64), each with one install job.
async fn mount_two_install_jobs(server: &MockServer, aarch64_status: &str) {
    Mock::given(method("GET"))
        .and(path("/api/incident_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 7, "settings": {}},
            {"id": 8, "settings": {}}
        ])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/update_settings/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    for (sid, job_id, arch, status) in [
        (7, 1001, "x86_64", "passed"),
        (8, 1002, "aarch64", aarch64_status),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/incident/{sid}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "job_id": job_id, "name": "qam-incidentinstall", "status": status,
                "distri": "sle", "flavor": "Server-DVD-Incidents", "arch": arch,
                "version": "15-SP5", "build": ":12358:bash", "obsolete": false
            }])))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn run_lists_install_logs_in_submission_order() {
    let server = MockServer::start().await;
    mount_two_install_jobs(&server, "passed").await;

    let mut dashboard = dashboard_for(&server).await;
    dashboard.run().await.unwrap();

    let urls: Vec<&str> = dashboard
        .results
        .as_ref()
        .expect("all install jobs passed -> results")
        .iter()
        .map(|u| u.url.as_str())
        .collect();
    assert_eq!(
        urls,
        [
            "https://openqa.example.com/tests/1001/file/update_install-zypper.log",
            "https://openqa.example.com/tests/1002/file/update_install-zypper.log",
        ]
    );
}

#[tokio::test]
async fn run_withholds_results_when_any_install_job_failed() {
    let server = MockServer::start().await;
    mount_two_install_jobs(&server, "failed").await;

    let mut dashboard = dashboard_for(&server).await;
    dashboard.run().await.unwrap();

    assert!(dashboard.results.is_none());
    assert!(dashboard.pp.concat().contains("Failed jobs:"));
}
