//! `AutoExport`'s install-log download, as seen in the log.

use async_trait::async_trait;
use mtui_config::options::Config;
use mtui_datasources::{DashboardAutoOpenQA, QemDashboardClient, QemIncident, VerifyPolicy};
use mtui_testreport::export::{AutoExport, BytesFetcher, ErrorMode, ExportContext, download_logs};
use mtui_types::{RequestReviewID, Test, URLs};

use crate::log_capture::capture_events_async;

struct FailingFetcher;

#[async_trait]
impl BytesFetcher for FailingFetcher {
    async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
        Err("boom".to_owned())
    }
}

/// The URL is built on the configured openQA instance, which may carry
/// credentials.
#[tokio::test]
async fn a_failed_install_log_download_logs_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.template_dir = dir.path().to_path_buf();
    let rrid: RequestReviewID = "SUSE:Maintenance:1:2".parse().unwrap();
    let incident = QemIncident {
        rrid: rrid.clone(),
        incident_number: "1".to_owned(),
        key: None,
        source: mtui_types::UpdateSource::Obs,
        client: QemDashboardClient::new(
            "http://dashboard.invalid/api",
            VerifyPolicy::Default(false),
        )
        .unwrap(),
        data: None,
    };
    let mut auto = DashboardAutoOpenQA::new("https://oqa.invalid", &incident, rrid.clone(), 1);
    auto.results = Some(vec![URLs::new(
        "SLES",
        "x86_64",
        "15-SP5",
        "",
        "https://user:hunter2@oqa.invalid/tests/1/file/log.txt",
        "passed",
    )]);
    let ex = AutoExport::new(ExportContext::new(cfg, false, rrid), Some(auto));

    let events = capture_events_async(async {
        assert!(ex.write_logs(&FailingFetcher).await.is_empty());
    })
    .await;

    assert!(
        events.contains("log https://***@oqa.invalid/tests/1/file/log.txt failed to download"),
        "{events:?}"
    );
    assert!(!events.contains("hunter2"), "{events:?}");
}

/// The kernel export's log URLs are built on the configured openQA instance
/// too; every line that names it must mask its credentials.
#[tokio::test]
async fn kernel_log_downloads_log_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let test = |name: &str| Test::new(name, "passed", 42, "x86_64", Default::default());
    let connectors = vec![(
        "https://user:hunter2@oqa.invalid/".to_owned(),
        vec![test("install_kernel"), test("noop"), test("install_../x")],
    )];

    let events = capture_events_async(async {
        download_logs(
            &FailingFetcher,
            &connectors,
            &dir.path().join("results"),
            &dir.path().join("install"),
            ErrorMode::Tolerant,
        )
        .await
        .unwrap();
    })
    .await;

    for expected in [
        "Downloading log https://***@oqa.invalid/tests/42/file/",
        "Download from https://***@oqa.invalid/tests/42/file/",
        "No log to download for test: noop on https://***@oqa.invalid/",
        "Refusing unsafe export path component for test \"install_../x\" (arch \"x86_64\") on https://***@oqa.invalid/",
    ] {
        assert!(events.contains(expected), "{expected}: {events:?}");
    }
    assert!(!events.contains("hunter2"), "{events:?}");
}
