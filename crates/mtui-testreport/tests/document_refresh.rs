//! Wiremock coverage for `mtui_testreport::refresh_document`: the conditional
//! re-fetch, adoption of a newer document, and the leave-untouched outcomes.

use std::collections::BTreeMap;
use std::str::FromStr;

use mtui_config::Config;
use mtui_datasources::teregen::{TeregenV2, TeregenV2Error};
use mtui_datasources::{HttpClient, VerifyPolicy};
use mtui_testreport::{
    PiReport, RefreshError, Refreshed, TestReport, apply_document, refresh_document,
};
use mtui_types::report_document::ReportDocument;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PI: &str = include_str!("../../mtui-types/tests/fixtures/document/pi.json");
const ID: &str = "SUSE:PI:16.0:1";
const OLD_ETAG: &str = "\"etag-old\"";
const NEW_ETAG: &str = "\"etag-new\"";

/// `reporepoparse` only keeps a repository whose URL names the product.
const REPO_BASE: &str = "http://download.suse.de/ibs/SLE-Product-SLES-16.0-x86_64";

fn document() -> ReportDocument {
    let mut doc = ReportDocument::from_str(PI).expect("fixture parses");
    doc.install.targets[0].repository = format!("{REPO_BASE}/older");
    doc
}

/// The fixture document with one more binary and a different repository URL,
/// so `packages` and `update_repos` both change when it is applied.
fn newer_document() -> ReportDocument {
    let mut doc = document();
    doc.install.targets[0].repository = format!("{REPO_BASE}/newer");
    doc.install.targets[0]
        .binaries
        .insert("newpkg".to_owned(), "2.0-1.1.x86_64".to_owned());
    doc
}

/// A PI report loaded from [`document`], with unsaved edits pending.
fn loaded_report() -> PiReport {
    let mut report = PiReport::new(Config::default());
    let doc = document();
    apply_document(report.base_mut(), &doc);
    let repos = report.update_repos_parser();
    let base = report.base_mut();
    base.update_repos = repos;
    base.document = Some(doc);
    base.document_etag = Some(OLD_ETAG.to_owned());
    base.document_dirty = true;
    report
}

fn client(server: &MockServer) -> TeregenV2 {
    let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
    TeregenV2::with_client(http, &server.uri())
}

/// Every field a refresh may touch, in a form that compares and prints.
fn state(report: &PiReport) -> String {
    let base = report.base();
    let packages: BTreeMap<_, _> = base.packages.iter().collect();
    let repos: BTreeMap<String, &String> = base
        .update_repos
        .iter()
        .map(|(k, v)| (format!("{k:?}"), v))
        .collect();
    format!(
        "{:?}|{:?}|{}|{packages:?}|{repos:?}|{}|{}",
        base.document, base.document_etag, base.document_dirty, base.repository, base.packager
    )
}

fn document_response(doc: &ReportDocument) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_string(serde_json::to_string(doc).unwrap())
        .insert_header("etag", NEW_ETAG)
}

#[tokio::test]
async fn sends_the_stored_etag_and_leaves_the_report_alone_on_304() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{ID}")))
        .and(header("if-none-match", OLD_ETAG))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;

    let mut report = loaded_report();
    let before = state(&report);

    let outcome = refresh_document(&mut report, &client(&server), false).await;

    assert!(matches!(outcome, Ok(Refreshed::Unchanged)), "{outcome:?}");
    assert_eq!(state(&report), before);
    assert!(
        report.base().document_dirty,
        "a 304 must not clear the flag"
    );
}

#[tokio::test]
async fn a_newer_document_is_adopted_reapplied_and_clears_the_flag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{ID}")))
        .respond_with(document_response(&newer_document()))
        .mount(&server)
        .await;

    let mut report = loaded_report();
    assert!(!format!("{:?}", report.base().packages).contains("newpkg"));
    let old_repos = format!("{:?}", report.base().update_repos);
    assert_ne!(old_repos, "{}", "the fixture must start with update repos");

    let outcome = refresh_document(&mut report, &client(&server), false)
        .await
        .expect("fresh document");

    assert_eq!(
        outcome,
        Refreshed::Updated {
            etag: Some(NEW_ETAG.to_owned())
        }
    );
    let base = report.base();
    assert_eq!(base.document.as_ref(), Some(&newer_document()));
    assert_eq!(base.document_etag.as_deref(), Some(NEW_ETAG));
    assert!(!base.document_dirty);
    assert!(
        format!("{:?}", base.packages).contains("newpkg"),
        "apply_document must re-derive packages: {:?}",
        base.packages
    );
    let new_repos = format!("{:?}", base.update_repos);
    assert_ne!(new_repos, old_repos, "update_repos must be re-derived");
    assert!(new_repos.contains("/newer"), "{new_repos}");
}

#[tokio::test]
async fn a_stale_document_is_refused_and_leaves_the_report_alone() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{ID}")))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;

    let mut report = loaded_report();
    let before = state(&report);

    let outcome = refresh_document(&mut report, &client(&server), false).await;

    assert!(
        matches!(outcome, Err(RefreshError::Fetch(TeregenV2Error::Stale))),
        "{outcome:?}"
    );
    assert_eq!(state(&report), before);
}

#[tokio::test]
async fn a_report_without_a_document_sends_nothing() {
    let server = MockServer::start().await;
    let mut report = PiReport::new(Config::default());

    let outcome = refresh_document(&mut report, &client(&server), false).await;

    assert!(
        matches!(outcome, Err(RefreshError::NoDocument)),
        "{outcome:?}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn force_fetches_unconditionally_and_replaces_the_local_document() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/reports/{ID}")))
        .respond_with(document_response(&newer_document()))
        .mount(&server)
        .await;

    let mut report = loaded_report();

    let outcome = refresh_document(&mut report, &client(&server), true).await;

    assert!(
        matches!(outcome, Ok(Refreshed::Updated { .. })),
        "{outcome:?}"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests[0].headers.get("if-none-match").is_none(),
        "{:?}",
        requests[0].headers
    );
    assert!(!report.base().document_dirty);
}
