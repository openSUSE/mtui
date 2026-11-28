//! `repo_check::check_repository` against a local rpm-md repository served by
//! `wiremock`: each verdict, the failures that must stay "no answer", and the
//! requests that must never leave the repository. Fixtures:
//! `tests/fixtures/rpm_md/` (hand-written, see its `PROVENANCE.md`).

use std::io::Write;

use mtui_datasources::repo_check::{
    Document, Listed, NoAnswer, PrimaryError, RepoCheck, RepomdError, check_repository,
};
use mtui_datasources::{HttpClient, VerifyPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPOMD: &str = include_str!("fixtures/rpm_md/repomd.xml");
const PRIMARY: &str = include_str!("fixtures/rpm_md/primary.xml");
const PRIMARY_AT: &str = "repodata/00-primary.xml.gz";

fn client() -> HttpClient {
    HttpClient::new(VerifyPolicy::Default(true)).expect("client builds")
}

fn listed(pairs: &[(&str, &str)]) -> Listed {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

/// The fixture `repomd.xml` with its primary at `href`.
fn repomd(href: &str) -> String {
    REPOMD.replace(PRIMARY_AT, href)
}

/// The fixture primary with no packages.
fn empty_primary() -> String {
    let start = PRIMARY.find("<package ").unwrap();
    let end = PRIMARY.rfind("</package>").unwrap() + "</package>\n".len();
    format!("{}{}", &PRIMARY[..start], &PRIMARY[end..])
        .replace(r#"packages="3""#, r#"packages="0""#)
}

fn gzip(text: &str) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(text.as_bytes()).unwrap();
    enc.finish().unwrap()
}

fn zstd(text: &str) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
}

async fn serve(server: &MockServer, at: &str, body: impl Into<Vec<u8>>) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.into()))
        .mount(server)
        .await;
}

async fn redirect(server: &MockServer, at: &str, location: &str) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(302).insert_header("location", location))
        .mount(server)
        .await;
}

/// A repository at `/repo/` serving the fixture repomd and `primary` gzipped.
async fn published(primary: &str) -> MockServer {
    let server = MockServer::start().await;
    serve(&server, "/repo/repodata/repomd.xml", REPOMD).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(primary)).await;
    server
}

async fn check(server: &MockServer, want: &Listed) -> RepoCheck {
    check_repository(&client(), &format!("{}/repo", server.uri()), want).await
}

/// The paths `server` was asked for.
async fn asked(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect()
}

fn no_answer(why: impl Into<NoAnswer>) -> RepoCheck {
    RepoCheck::NoAnswer(why.into())
}

#[tokio::test]
async fn every_listed_binary_present_is_ok() {
    let server = published(PRIMARY).await;
    let got = check(
        &server,
        &listed(&[("foo", "1.2-3.1.x86_64"), ("foo-doc", "1.2-3.1.noarch")]),
    )
    .await;
    assert_eq!(
        got,
        RepoCheck::Ok {
            packages: 3,
            listed: 2
        }
    );
    assert_eq!(got.to_string(), "3 packages, all 2 listed binaries present");
}

#[tokio::test]
async fn a_zstd_primary_is_read() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        repomd("repodata/00-primary.xml.zst"),
    )
    .await;
    serve(&server, "/repo/repodata/00-primary.xml.zst", zstd(PRIMARY)).await;
    let got = check(&server, &listed(&[("foo-devel", "1.2-3.1.x86_64")])).await;
    assert_eq!(
        got,
        RepoCheck::Ok {
            packages: 3,
            listed: 1
        }
    );
}

#[tokio::test]
async fn a_primary_from_the_reference_zstd_encoder_is_read() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        repomd("repodata/00-primary.xml.zst"),
    )
    .await;
    serve(
        &server,
        "/repo/repodata/00-primary.xml.zst",
        include_bytes!("fixtures/rpm_md/primary.xml.zst").to_vec(),
    )
    .await;
    let got = check(&server, &listed(&[("foo-doc", "1.2-3.1.noarch")])).await;
    assert_eq!(
        got,
        RepoCheck::Ok {
            packages: 3,
            listed: 1
        }
    );
}

#[tokio::test]
async fn a_truncated_zstd_primary_is_no_answer() {
    let server = MockServer::start().await;
    let full = zstd(PRIMARY);
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        repomd("repodata/00-primary.xml.zst"),
    )
    .await;
    serve(
        &server,
        "/repo/repodata/00-primary.xml.zst",
        full[..full.len() / 2].to_vec(),
    )
    .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(PrimaryError::Unreadable));
}

#[tokio::test]
async fn an_uncompressed_primary_is_read() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        repomd("repodata/00-primary.xml"),
    )
    .await;
    serve(&server, "/repo/repodata/00-primary.xml", PRIMARY).await;
    let got = check(&server, &listed(&[("foo", "1.2-3.1.x86_64")])).await;
    assert_eq!(got.packages(), Some(3), "{got:?}");
    assert_eq!(got.status().as_str(), "ok");
}

#[tokio::test]
async fn a_binary_at_another_version_or_absent_is_missing() {
    let server = published(PRIMARY).await;
    let got = check(
        &server,
        &listed(&[
            ("foo", "1.2-3.1.x86_64"),
            ("foo-devel", "1.2-4.1.x86_64"),
            ("libfoo1", "1.2-3.1.x86_64"),
        ]),
    )
    .await;
    assert_eq!(
        got,
        RepoCheck::Missing {
            packages: 3,
            missing: vec!["foo-devel".to_owned(), "libfoo1".to_owned()],
        }
    );
    assert_eq!(
        got.to_string(),
        "3 packages; not published at the listed version-release.arch: foo-devel, libfoo1"
    );
}

#[tokio::test]
async fn a_repository_no_target_lists_is_checked_for_packages_only() {
    let server = published(PRIMARY).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(
        got.to_string(),
        "3 packages; no install target lists binaries for this repository, so only checked \
         that it is published and non-empty"
    );
}

#[tokio::test]
async fn zero_packages_is_empty() {
    let server = published(&empty_primary()).await;
    let got = check(&server, &listed(&[("foo", "1.2-3.1.x86_64")])).await;
    assert_eq!(got, RepoCheck::Empty);
    assert_eq!(got.packages(), Some(0));
}

#[tokio::test]
async fn repomd_404_is_not_published() {
    let server = MockServer::start().await;
    let got = check(&server, &listed(&[("foo", "1.2-3.1.x86_64")])).await;
    assert_eq!(got, RepoCheck::NotPublished);
    assert_eq!(got.to_string(), "repodata/repomd.xml answered 404");
}

#[tokio::test]
async fn a_server_error_is_no_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::Status(Document::Repomd, 503)));
    assert_eq!(got.to_string(), "repodata/repomd.xml answered HTTP 503");
}

#[tokio::test]
async fn repomd_without_a_primary_entry_is_no_answer() {
    let server = MockServer::start().await;
    let body = REPOMD.replace(r#"type="primary""#, r#"type="other""#);
    serve(&server, "/repo/repodata/repomd.xml", body).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::NoPrimary));
}

#[tokio::test]
async fn a_primary_that_answers_404_is_no_answer_not_unpublished() {
    let server = MockServer::start().await;
    serve(&server, "/repo/repodata/repomd.xml", REPOMD).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::Status(Document::Primary, 404)));
}

#[tokio::test]
async fn a_truncated_repomd_is_no_answer() {
    let server = MockServer::start().await;
    // Cut after the primary location: everything a lenient reader needs is there.
    let cut = REPOMD.find("<timestamp>").unwrap();
    serve(&server, "/repo/repodata/repomd.xml", &REPOMD[..cut]).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::Unreadable));
}

#[tokio::test]
async fn an_html_answer_is_no_answer() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        "<html><body>Not here</body></html>",
    )
    .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::Unreadable));
}

#[tokio::test]
async fn a_malformed_attribute_is_unreadable_not_absent() {
    let server = MockServer::start().await;
    // An unquoted value makes the attribute list malformed.
    let body = REPOMD.replace(r#"<data type="primary">"#, r#"<data type=primary>"#);
    serve(&server, "/repo/repodata/repomd.xml", body).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::Unreadable));
}

#[tokio::test]
async fn a_truncated_primary_is_no_answer_never_missing_or_empty() {
    // Two whole packages survive, without `</metadata>`; `packages="3"` is
    // dropped so only the missing root close can catch it.
    let cut = PRIMARY.match_indices("<package ").nth(2).unwrap().0;
    let truncated = PRIMARY[..cut].replace(r#" packages="3""#, "");
    let server = published(&truncated).await;
    let got = check(&server, &listed(&[("foo-doc", "1.2-3.1.noarch")])).await;
    assert_eq!(got, no_answer(PrimaryError::Unreadable));
}

#[tokio::test]
async fn a_truncated_gzip_stream_is_no_answer() {
    let server = MockServer::start().await;
    let full = gzip(PRIMARY);
    serve(&server, "/repo/repodata/repomd.xml", REPOMD).await;
    serve(
        &server,
        &format!("/repo/{PRIMARY_AT}"),
        full[..full.len() / 2].to_vec(),
    )
    .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(PrimaryError::Unreadable));
}

#[tokio::test]
async fn a_package_count_that_disagrees_is_no_answer() {
    let server = published(&PRIMARY.replace(r#"packages="3""#, r#"packages="4""#)).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(
        got,
        no_answer(PrimaryError::CountMismatch {
            declared: 4,
            listed: 3
        })
    );
}

#[tokio::test]
async fn xz_and_bzip2_primaries_are_no_answer_naming_the_compression() {
    for (magic, name) in [
        (&[0xfd, b'7', b'z', b'X', b'Z', 0x00, 0x00][..], "xz"),
        (&b"BZh91AY&SY"[..], "bzip2"),
    ] {
        let server = MockServer::start().await;
        serve(&server, "/repo/repodata/repomd.xml", REPOMD).await;
        serve(&server, &format!("/repo/{PRIMARY_AT}"), magic.to_vec()).await;
        let got = check(&server, &Listed::new()).await;
        assert_eq!(got, no_answer(PrimaryError::Unsupported(name)));
        assert_eq!(
            got.to_string(),
            format!(
                "the primary metadata is {name}-compressed, which this build cannot \
                 decompress (gzip, zstd and uncompressed only)"
            )
        );
    }
}

#[tokio::test]
async fn a_primary_location_on_another_origin_is_not_fetched() {
    let server = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    // Same path as the repository's own, so only the origin tells them apart.
    serve(&elsewhere, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    serve(
        &server,
        "/repo/repodata/repomd.xml",
        repomd(&format!("{}/repo/{PRIMARY_AT}", elsewhere.uri())),
    )
    .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::Outside));
    assert!(asked(&elsewhere).await.is_empty());
}

#[tokio::test]
async fn encoded_traversal_in_the_location_is_not_fetched() {
    for href in [
        "repodata/..%2f..%2fother/p.xml",
        "repodata/..%2F..%2Fother/p.xml",
        "repodata/%2e%2e/%2e%2e/other/p.xml",
        "repodata/.%2e/.%2e/other/p.xml",
        "repodata/..%5c..%5cother/p.xml",
    ] {
        let server = MockServer::start().await;
        serve(&server, "/repo/repodata/repomd.xml", repomd(href)).await;
        let got = check(&server, &Listed::new()).await;
        assert_eq!(got, no_answer(RepomdError::Outside), "{href}");
        assert_eq!(
            asked(&server).await,
            ["/repo/repodata/repomd.xml"],
            "{href}"
        );
    }
}

#[tokio::test]
async fn a_primary_location_with_an_xml_base_is_not_fetched() {
    let server = MockServer::start().await;
    let body = REPOMD.replace(
        &format!(r#"<location href="{PRIMARY_AT}"/>"#),
        &format!(r#"<location xml:base="http://elsewhere.invalid/" href="{PRIMARY_AT}"/>"#),
    );
    serve(&server, "/repo/repodata/repomd.xml", body).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(RepomdError::ForeignBase));
}

#[tokio::test]
async fn a_redirect_inside_the_repository_is_followed_with_the_credentials() {
    let server = MockServer::start().await;
    redirect(
        &server,
        "/repo/repodata/repomd.xml",
        "/repo/moved/repomd.xml",
    )
    .await;
    serve(&server, "/repo/moved/repomd.xml", REPOMD).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    let url = server.uri().replace("http://", "http://alice:s3cret@") + "/repo";
    let got = check_repository(&client(), &url, &Listed::new()).await;
    assert_eq!(got.status().as_str(), "ok", "{got:?}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[1].url.path(), "/repo/moved/repomd.xml");
    assert!(
        requests
            .iter()
            .all(|r| r.headers.get("authorization").is_some())
    );
}

#[tokio::test]
async fn an_absolute_redirect_without_userinfo_keeps_the_credentials() {
    let server = MockServer::start().await;
    let location = format!("{}/repo/moved/repomd.xml", server.uri());
    redirect(&server, "/repo/repodata/repomd.xml", &location).await;
    serve(&server, "/repo/moved/repomd.xml", REPOMD).await;
    serve(&server, &format!("/repo/{PRIMARY_AT}"), gzip(PRIMARY)).await;
    let url = server.uri().replace("http://", "http://alice:s3cret@") + "/repo";
    let got = check_repository(&client(), &url, &Listed::new()).await;
    assert_eq!(got.status().as_str(), "ok", "{got:?}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[1].url.path(), "/repo/moved/repomd.xml");
    assert!(requests[1].headers.get("authorization").is_some());
}

#[tokio::test]
async fn a_redirect_to_another_origin_is_refused_and_never_followed() {
    let origin = MockServer::start().await;
    let mirror = published(PRIMARY).await;
    redirect(
        &origin,
        "/repo/repodata/repomd.xml",
        &format!("{}/repo/repodata/repomd.xml", mirror.uri()),
    )
    .await;
    let url = origin.uri().replace("http://", "http://alice:s3cret@") + "/repo";
    let got = check_repository(&client(), &url, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::RedirectOutside(Document::Repomd)));
    assert!(asked(&mirror).await.is_empty());
}

#[tokio::test]
async fn a_redirect_of_the_primary_outside_the_repository_is_refused() {
    let server = MockServer::start().await;
    serve(&server, "/repo/repodata/repomd.xml", REPOMD).await;
    redirect(&server, &format!("/repo/{PRIMARY_AT}"), "/other/p.xml.gz").await;
    serve(&server, "/other/p.xml.gz", gzip(PRIMARY)).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::RedirectOutside(Document::Primary)));
    assert!(!asked(&server).await.contains(&"/other/p.xml.gz".to_owned()));
}

#[tokio::test]
async fn a_redirect_loop_stops() {
    let server = MockServer::start().await;
    redirect(
        &server,
        "/repo/repodata/repomd.xml",
        "/repo/repodata/repomd.xml",
    )
    .await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::TooManyRedirects(Document::Repomd)));
    assert_eq!(asked(&server).await.len(), 6);
}

#[tokio::test]
async fn credentials_in_a_redirect_never_reach_the_reason() {
    // Same host and path, other userinfo: refused, and the Location (which
    // carries the credential) must not be echoed.
    let server = MockServer::start().await;
    let location = server.uri().replace("http://", "http://mallory:s3cret@") + "/repo/x.xml";
    redirect(&server, "/repo/repodata/repomd.xml", &location).await;
    let got = check(&server, &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::RedirectOutside(Document::Repomd)));
    let reason = got.to_string();
    assert!(
        !reason.contains("s3cret") && !reason.contains("mallory"),
        "{reason}"
    );
}

#[tokio::test]
async fn credentials_in_the_url_never_reach_a_transport_reason() {
    // Nothing listens on port 9 here; the transport error carries the cause.
    let url = "http://alice:s3cret@127.0.0.1:9/repo";
    let got = check_repository(&client(), url, &Listed::new()).await;
    let reason = got.to_string();
    assert!(
        reason.starts_with("fetching repodata/repomd.xml failed: "),
        "{reason}"
    );
    assert!(
        !reason.contains("s3cret") && !reason.contains("alice"),
        "{reason}"
    );
}

#[tokio::test]
async fn an_invalid_url_is_no_answer() {
    let got = check_repository(&client(), "ftp://h/repo", &Listed::new()).await;
    assert_eq!(got, no_answer(NoAnswer::InvalidUrl));
}
