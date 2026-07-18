//! Integration tests for the QEM Dashboard connector, exercising the public
//! `QemIncident` + `DashboardAutoOpenQA` API end-to-end against a mock server.
//!
//! Ported from `tests/test_qem_dashboard_connector.py` (the `@responses`
//! end-to-end cases). The `_pretty_print` / pure-helper and async-timeout
//! assertions are colocated unit tests in `src/qem_dashboard/`.

use mtui_datasources::http::{HttpClient, VerifyPolicy};
use mtui_datasources::qem_dashboard::{DashboardAutoOpenQA, QemDashboardClient, QemIncident};
use mtui_types::RequestReviewID;
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
    // Ported from test_dashboard_auto_openqa_loads_incident_and_aggregate_jobs.
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
    let incident = QemIncident::with_client(rrid.clone(), client_for(&server)).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid);
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
    // Ported from test_qem_incident_uses_review_id_for_slfo_1_2: the incident
    // number for an SLFO 1.2 request is the review id, so every dashboard URL
    // keys on it.
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
    let incident = QemIncident::with_client(rrid.clone(), client_for(&server)).await;
    assert_eq!(incident.incident_number, "12358");

    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid);
    dashboard.run().await.unwrap();
    // No jobs -> no results, no rendered block.
    assert!(dashboard.pp.is_empty());
    assert!(dashboard.results.is_none());
    assert!(!dashboard.is_present());
}

#[tokio::test]
async fn incident_metadata_shortest_package_name() {
    // Ported from test_qem_incident_metadata (public-API path).
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
    let incident = QemIncident::with_client(rrid, client_for(&server)).await;
    assert!(incident.is_present());
    assert_eq!(incident.get_incident_name().as_deref(), Some("kernel-ec2"));
}

#[tokio::test]
async fn oversized_incident_body_folds_to_absent() {
    // th4o.9: a body exceeding MAX_API_BODY is rejected by the bounded read and
    // the dashboard `get` folds that error to None, so an oversized/hostile
    // response degrades to "incident not present" instead of OOMing.
    use mtui_datasources::MAX_API_BODY;
    let server = MockServer::start().await;
    let oversized = vec![b'x'; MAX_API_BODY + 1];
    Mock::given(method("GET"))
        .and(path("/api/incidents/12358"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
        .mount(&server)
        .await;

    let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
    let incident = QemIncident::with_client(rrid, client_for(&server)).await;
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
    let incident = QemIncident::with_client(rrid.clone(), client_for(&server)).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid);
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
    let incident = QemIncident::with_client(rrid.clone(), client_for(&server)).await;
    let mut dashboard = DashboardAutoOpenQA::new(OPENQA_HOST, &incident, rrid);
    dashboard.run().await.unwrap();
    assert!(dashboard.results.is_none());
    assert!(dashboard.pp.is_empty());
}
